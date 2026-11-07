//! The seccomp filter a bubblewrap run gets (`bwrap --seccomp`), which refuses it Unix sockets:
//! mounts hide files, but a socket is how a process asks a service outside the sandbox
//! (`systemd-run --user` over the user bus, Docker, ssh-agent) to run something for it, and an
//! abstract socket has no path a mount could hide. Claude Code's sandbox refuses them the same
//! way.

use std::path::Path;

/// The filter's file name in the run root, where mothership writes it at startup.
pub const FILE: &str = "seccomp.bpf";

/// `struct seccomp_data` offsets.
const NR: u32 = 0;
const ARCH: u32 = 4;
/// The low half of the first argument, on a little-endian machine.
const ARG0: u32 = 16;

const LD_W_ABS: u16 = 0x20;
const JEQ: u16 = 0x15;
const JGE: u16 = 0x35;
const RET: u16 = 0x06;

const ALLOW: u32 = 0x7fff_0000;
/// `SECCOMP_RET_ERRNO | EPERM`.
const REFUSE: u32 = 0x0005_0001;
const AF_UNIX: u32 = 1;
/// Syscall numbers with this bit set are x32's, which would reach `socket` by another number.
const X32: u32 = 0x4000_0000;
/// `io_uring_setup`, `io_uring_enter` and `io_uring_register`, the same on every architecture:
/// `io_uring`'s own socket operation never passes through seccomp.
const IO_URING: [u32; 3] = [425, 426, 427];

#[cfg(target_arch = "x86_64")]
const NATIVE: Option<(u32, u32)> = Some((0xc000_003e, 41));
#[cfg(target_arch = "aarch64")]
const NATIVE: Option<(u32, u32)> = Some((0xc000_00b7, 198));
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const NATIVE: Option<(u32, u32)> = None;

/// The filter as `bwrap --seccomp` reads it: `struct sock_filter`s in native byte order.
/// `socket(AF_UNIX, …)` and `io_uring` fail with `EPERM`, and so does every call from another
/// architecture's ABI (a 32-bit process's `socketcall` hides its arguments). None on an
/// architecture it has no syscall numbers for, where a run must not start.
pub fn filter() -> Option<Vec<u8>> {
    let (arch, socket) = NATIVE?;
    Some(program(arch, socket).into_iter().flat_map(encode).collect())
}

/// `(code, jump if true, jump if false, k)`; jumps count instructions skipped.
type Insn = (u16, u8, u8, u32);

fn program(arch: u32, socket: u32) -> Vec<Insn> {
    // The last two instructions: allow at 10, refuse at 11. Each jump skips to one of them.
    vec![
        (LD_W_ABS, 0, 0, ARCH),
        (JEQ, 0, 9, arch),
        (LD_W_ABS, 0, 0, NR),
        (JGE, 7, 0, X32),
        (JEQ, 6, 0, IO_URING[0]),
        (JEQ, 5, 0, IO_URING[1]),
        (JEQ, 4, 0, IO_URING[2]),
        (JEQ, 0, 2, socket),
        (LD_W_ABS, 0, 0, ARG0),
        (JEQ, 1, 0, AF_UNIX),
        (RET, 0, 0, ALLOW),
        (RET, 0, 0, REFUSE),
    ]
}

fn encode((code, jt, jf, k): Insn) -> Vec<u8> {
    let mut bytes = code.to_ne_bytes().to_vec();
    bytes.extend([jt, jf]);
    bytes.extend(k.to_ne_bytes());
    bytes
}

/// `bwrap` with `args`, started through `sh` so `filter` is open on fd 3 for `--seccomp 3`:
/// `std` cannot hand a child any other descriptor without `unsafe`.
pub fn wrapped(bwrap: &str, filter: &Path, args: Vec<String>) -> Vec<String> {
    let mut all: Vec<String> = ["/bin/sh", "-c", r#"exec "$@" 3<"$0""#]
        .map(String::from)
        .to_vec();
    all.extend([
        filter.display().to_string(),
        bwrap.into(),
        "--seccomp".into(),
        "3".into(),
    ]);
    all.extend(args);
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCH_X: u32 = 0xc000_003e;

    /// What the filter answers for a call, run as the kernel runs a classic BPF program.
    fn answer(arch: u32, nr: u32, arg0: u32) -> u32 {
        let prog = program(ARCH_X, 41);
        let (mut pc, mut acc) = (0, 0);
        loop {
            let (code, jt, jf, k) = prog[pc];
            pc += 1;
            match code {
                LD_W_ABS => {
                    acc = match k {
                        NR => nr,
                        ARCH => arch,
                        ARG0 => arg0,
                        _ => panic!("loads only nr, arch and arg0"),
                    }
                }
                JEQ | JGE => {
                    let hit = if code == JEQ { acc == k } else { acc >= k };
                    pc += usize::from(if hit { jt } else { jf });
                }
                RET => return k,
                _ => panic!("unknown instruction {code:#x}"),
            }
        }
    }

    #[test]
    fn refuses_unix_sockets_only() {
        assert_eq!(answer(ARCH_X, 41, AF_UNIX), REFUSE);
        assert_eq!(answer(ARCH_X, 41, 2), ALLOW, "AF_INET");
        assert_eq!(answer(ARCH_X, 41, 10), ALLOW, "AF_INET6");
        assert_eq!(answer(ARCH_X, 53, AF_UNIX), ALLOW, "socketpair");
        assert_eq!(answer(ARCH_X, 0, AF_UNIX), ALLOW, "read");
        for nr in IO_URING {
            assert_eq!(answer(ARCH_X, nr, 0), REFUSE);
        }
        assert_eq!(answer(ARCH_X, X32 | 0x29, AF_UNIX), REFUSE, "x32");
        assert_eq!(answer(0x4000_0003, 102, 0), REFUSE, "i386");
        let bytes = filter().expect("tests run on x86_64 or aarch64");
        assert_eq!(bytes.len(), program(0, 0).len() * 8);
    }

    /// The filter and mounts as bubblewrap applies them, on this machine.
    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_closes_the_ways_out() {
        use std::process::Command;
        const BWRAP: &str = "/usr/bin/bwrap";
        // bubblewrap cannot start under an agent's own sandbox, nor where it is missing or
        // user namespaces are off. Only that skips; any other failure fails.
        let plain = Command::new(BWRAP)
            .args(["--ro-bind", "/", "/", "/bin/true"])
            .output();
        if let Some(why) = match &plain {
            Err(e) => Some(e.to_string()),
            Ok(out) if !out.status.success() => Some(String::from_utf8_lossy(&out.stderr).into()),
            Ok(_) => None,
        } {
            eprintln!("skipped: bwrap cannot run here: {why}");
            return;
        }
        let base = std::env::temp_dir().join(format!("mothership-sc-{}", std::process::id()));
        std::fs::create_dir_all(base.join("ws")).unwrap();
        std::fs::create_dir_all(base.join("own")).unwrap();
        let d = std::fs::canonicalize(&base).unwrap();
        let file = d.join("own/seccomp.bpf");
        std::fs::write(&file, filter().unwrap()).unwrap();
        let settings = serde_json::json!({});
        let made = wrapped(
            BWRAP,
            &file,
            super::super::bwrap::args(&settings, &[d.join("ws")], &d.join("own"), None),
        );
        let sandboxed = |cmd: &str| {
            Command::new(&made[0])
                .args(&made[1..])
                .args(["/bin/sh", "-c", cmd])
                .current_dir(&d)
                .status()
                .unwrap()
                .success()
        };
        let free = |cmd: &str| {
            Command::new("/bin/sh")
                .args(["-c", cmd])
                .status()
                .unwrap()
                .success()
        };
        assert!(sandboxed("echo > ws/a"));
        let socket =
            |family| format!("perl -MSocket -e 'socket(S, {family}, SOCK_STREAM, 0) or exit 1'");
        if free(&socket("AF_UNIX")) {
            assert!(!sandboxed(&socket("AF_UNIX")));
            assert!(sandboxed(&socket("AF_INET")));
        } else {
            eprintln!("skipped the socket check: no perl to open one");
        }
        // The escape this filter closes: a command the user's systemd runs outside the sandbox.
        let escape = "systemd-run --user --pipe --quiet true";
        if free(escape) {
            assert!(!sandboxed(escape));
        } else {
            eprintln!("skipped the systemd-run check: no user manager to ask");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
