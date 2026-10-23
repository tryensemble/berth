//! The seccomp filter every process in a berth runs under.
//!
//! It refuses the system calls a berth has no use for and that reach the most
//! kernel code: loading modules or kernels, BPF, perf, io_uring, the kernel
//! keyring, userfaultfd, clock and swap changes. Everything a Linux machine
//! normally does, Docker included, still works: a berth's root is already
//! confined by its user namespace, so the filter's job is to shrink the kernel
//! surface a berth can reach, not to take away what root in it may do.
//!
//! A denylist alone would let through whatever system calls kernels add after
//! it was written, so calls numbered past the newest one it was reviewed
//! against fail with ENOSYS too, as on an older kernel, until they're looked
//! at and added.
//!
//! The filter is plain classic BPF, built here, so berth needs no libseccomp.

use std::io;

const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;

// Classic BPF opcodes.
const LD_W_ABS: u16 = 0x20;
const JEQ_K: u16 = 0x15;
const JGT_K: u16 = 0x25;
const JGE_K: u16 = 0x35;
const RET_K: u16 = 0x06;

// Offsets in struct seccomp_data.
const NR: u32 = 0;
const ARCH: u32 = 4;
/// The low half of the first argument (little-endian).
const ARG0_LO: u32 = 16;

/// The newest system call the filter was reviewed against: listns, Linux
/// 7.0's last (x86_64 and arm64 number new calls alike). Anything after it
/// fails with ENOSYS until it's reviewed and this moves on.
pub const NEWEST_REVIEWED: u32 = 470;

/// The personalities a berth may take: Linux, 32-bit, the 2.6 uname, and no
/// address randomization (gdb), alone or together, and the query. Not those
/// that map page zero or make reads imply exec.
pub const PERSONALITIES: &[u32] = &[0x0, 0x8, 0x2_0000, 0x2_0008, 0x4_0000, 0x4_0008, 0x6_0000, 0x6_0008, 0xffff_ffff];

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;

/// The x32 ABI's syscalls on x86_64: an older, rarely used path into the
/// kernel, refused whole.
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// Kernel subsystems a berth doesn't get. They fail with ENOSYS, as on a
/// kernel built without them, which programs take as "fall back": runc carries
/// on without a session keyring, libuv without io_uring.
pub fn absent() -> Vec<libc::c_long> {
    vec![
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
    ]
}

/// What a berth may not do. These fail with EPERM.
pub fn denied() -> Vec<libc::c_long> {
    let mut v = vec![
        // Loading code into the kernel, or another kernel.
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        // File handles skip path permission checks.
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        // The machine's clock, swap, accounting and quotas aren't a berth's.
        libc::SYS_settimeofday,
        libc::SYS_clock_settime,
        libc::SYS_clock_adjtime,
        libc::SYS_adjtimex,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_reboot,
        libc::SYS_syslog,
        // Comparing kernel objects across processes, and moving other
        // processes' memory or pinning it to NUMA nodes: refused as in
        // Docker's default profile.
        libc::SYS_kcmp,
        libc::SYS_mbind,
        libc::SYS_set_mempolicy,
        libc::SYS_set_mempolicy_home_node,
        libc::SYS_migrate_pages,
        libc::SYS_move_pages,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend([libc::SYS_iopl, libc::SYS_ioperm, libc::SYS_lookup_dcookie, libc::SYS_uselib]);
    v
}

/// The filter program: other architectures' calls, calls newer than the
/// review and absent subsystems fail with ENOSYS, denied calls and other
/// personalities with EPERM, and everything else is allowed.
pub fn program() -> Vec<libc::sock_filter> {
    let stmt = |code, k| libc::sock_filter { code, jt: 0, jf: 0, k };
    let jump = |code, k, jt, jf| libc::sock_filter { code, jt, jf, k };
    let mut p = vec![
        stmt(LD_W_ABS, ARCH),
        jump(JEQ_K, AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_ERRNO | libc::ENOSYS as u32),
        stmt(LD_W_ABS, NR),
    ];
    #[cfg(target_arch = "x86_64")]
    {
        p.push(jump(JGE_K, X32_SYSCALL_BIT, 0, 1));
        p.push(stmt(RET_K, RET_ERRNO | libc::ENOSYS as u32));
    }
    p.push(jump(JGT_K, NEWEST_REVIEWED, 0, 1));
    p.push(stmt(RET_K, RET_ERRNO | libc::ENOSYS as u32));
    // personality(2): only the listed ones, by its argument.
    let n = PERSONALITIES.len() as u8;
    p.push(jump(JEQ_K, libc::SYS_personality as u32, 0, n + 3));
    p.push(stmt(LD_W_ABS, ARG0_LO));
    for (i, v) in PERSONALITIES.iter().enumerate() {
        p.push(jump(JEQ_K, *v, n - i as u8, 0));
    }
    p.push(stmt(RET_K, RET_ERRNO | libc::EPERM as u32));
    p.push(stmt(RET_K, RET_ALLOW));
    for (calls, errno) in [(absent(), libc::ENOSYS), (denied(), libc::EPERM)] {
        for nr in calls {
            p.push(jump(JEQ_K, nr as u32, 0, 1));
            p.push(stmt(RET_K, RET_ERRNO | errno as u32));
        }
    }
    p.push(stmt(RET_K, RET_ALLOW));
    p
}

/// Puts this process (all its threads) and everything it starts from now on
/// under the filter. Needs CAP_SYS_ADMIN in its user namespace, or
/// no_new_privs. Allocation-free, so it's safe between fork and exec once
/// prog is built.
pub fn install(prog: &[libc::sock_filter]) -> io::Result<()> {
    const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
    const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;
    let fprog = libc::sock_fprog { len: prog.len() as u16, filter: prog.as_ptr() as *mut _ };
    let r = unsafe {
        libc::syscall(libc::SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, &fprog as *const _)
    };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Runs check in a child under the filter, then execs true: the child
    /// succeeds only if check holds and ordinary programs still run.
    fn under_filter(check: fn() -> io::Result<()>) -> io::Result<std::process::ExitStatus> {
        let prog = program();
        let mut cmd = Command::new("true");
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                install(&prog)?;
                check()
            });
        }
        cmd.status()
    }

    fn errno_of(r: libc::c_long) -> Option<i32> {
        (r == -1).then(|| io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    /// Runs call under the filter and returns the errno it failed with.
    fn errno_under_filter(call: fn() -> libc::c_long) -> Option<i32> {
        // The child reports the errno by failing its spawn with it.
        let mut cmd = Command::new("true");
        let prog = program();
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                install(&prog)?;
                match errno_of(call()) {
                    Some(e) => Err(io::Error::from_raw_os_error(e)),
                    None => Ok(()),
                }
            });
        }
        cmd.status().err().and_then(|e| e.raw_os_error())
    }

    #[test]
    fn absent_subsystems_fail_with_enosys() {
        let calls: [fn() -> libc::c_long; 4] = [
            || unsafe { libc::syscall(libc::SYS_bpf, 0, 0, 0) },
            || unsafe { libc::syscall(libc::SYS_keyctl, 0, 0, 0) },
            || unsafe { libc::syscall(libc::SYS_io_uring_setup, 1, 0) },
            || unsafe { libc::syscall(libc::SYS_userfaultfd, 0) },
        ];
        for (i, call) in calls.into_iter().enumerate() {
            assert_eq!(errno_under_filter(call), Some(libc::ENOSYS), "call {i}");
        }
    }

    #[test]
    fn privileged_operations_fail_with_eperm() {
        let calls: [fn() -> libc::c_long; 2] = [
            || unsafe { libc::syscall(libc::SYS_init_module, 0, 0, 0) },
            || unsafe { libc::syscall(libc::SYS_swapoff, c"/x".as_ptr()) },
        ];
        for (i, call) in calls.into_iter().enumerate() {
            assert_eq!(errno_under_filter(call), Some(libc::EPERM), "call {i}");
        }
    }

    #[test]
    fn ordinary_calls_still_work() {
        let status = under_filter(|| {
            let pid = unsafe { libc::syscall(libc::SYS_getpid) };
            let fd = unsafe { libc::syscall(libc::SYS_memfd_create, c"t".as_ptr(), 0) };
            if pid > 0 && fd >= 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        })
        .expect("spawn under the filter");
        assert!(status.success());
    }

    #[test]
    fn every_refused_call_has_a_rule() {
        let p = program();
        for nr in absent().into_iter().chain(denied()) {
            assert!(p.iter().any(|f| f.code == JEQ_K && f.k == nr as u32), "{nr}");
        }
        assert_eq!(p.last().unwrap().k, RET_ALLOW);
        assert!(p.len() < 4096, "BPF programs are capped at 4096 instructions");
    }

    /// Runs the program over one call the way the kernel would, for the
    /// opcodes it uses: what it returns for arch, nr and the first argument.
    fn run(p: &[libc::sock_filter], arch: u32, nr: u32, arg0: u64) -> u32 {
        let load = |k| match k {
            NR => nr,
            ARCH => arch,
            ARG0_LO => arg0 as u32,
            _ => panic!("load of offset {k}"),
        };
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let f = p[pc];
            pc += 1;
            let taken = match f.code {
                LD_W_ABS => {
                    acc = load(f.k);
                    continue;
                }
                RET_K => return f.k,
                JEQ_K => acc == f.k,
                JGT_K => acc > f.k,
                JGE_K => acc >= f.k,
                c => panic!("opcode {c:#x}"),
            };
            pc += if taken { f.jt } else { f.jf } as usize;
        }
    }

    const ENOSYS: u32 = RET_ERRNO | libc::ENOSYS as u32;
    const EPERM: u32 = RET_ERRNO | libc::EPERM as u32;

    #[test]
    fn the_program_decides_each_call_as_intended() {
        let p = program();
        let call = |nr: libc::c_long, arg0: u64| run(&p, AUDIT_ARCH, nr as u32, arg0);
        assert_eq!(call(libc::SYS_read, 0), RET_ALLOW);
        assert_eq!(call(libc::SYS_mseal, 0), RET_ALLOW);
        assert_eq!(call(libc::SYS_bpf, 0), ENOSYS);
        assert_eq!(call(libc::SYS_kcmp, 0), EPERM);
        assert_eq!(call(libc::SYS_set_mempolicy, 0), EPERM);
        // Calls newer than the review don't exist, as far as a berth knows.
        assert_eq!(call(NEWEST_REVIEWED as libc::c_long, 0), RET_ALLOW);
        assert_eq!(call(NEWEST_REVIEWED as libc::c_long + 1, 0), ENOSYS);
        assert_eq!(call(1000, 0), ENOSYS);
        // Another architecture's calls.
        assert_eq!(run(&p, 0x4000_0003, libc::SYS_read as u32, 0), ENOSYS);
    }

    #[test]
    fn only_harmless_personalities_are_taken() {
        let p = program();
        let personality = |v: u64| run(&p, AUDIT_ARCH, libc::SYS_personality as u32, v);
        for v in PERSONALITIES {
            assert_eq!(personality(*v as u64), RET_ALLOW, "{v:#x}");
        }
        const READ_IMPLIES_EXEC: u64 = 0x40_0000;
        const MMAP_PAGE_ZERO: u64 = 0x10_0000;
        for v in [READ_IMPLIES_EXEC, MMAP_PAGE_ZERO, 0x4_0000 | MMAP_PAGE_ZERO, 0x1] {
            assert_eq!(personality(v), EPERM, "{v:#x}");
        }
        // The kernel reads the argument as 32 bits, and so does the filter.
        assert_eq!(personality(1 << 32), RET_ALLOW);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_calls_are_refused() {
        assert_eq!(run(&program(), AUDIT_ARCH, X32_SYSCALL_BIT | libc::SYS_read as u32, 0), ENOSYS);
    }

    #[test]
    fn the_kernel_agrees_about_personalities() {
        // gdb turns address randomization off for what it debugs.
        assert_eq!(errno_under_filter(|| unsafe { libc::syscall(libc::SYS_personality, 0x4_0000) }), None);
        assert_eq!(
            errno_under_filter(|| unsafe { libc::syscall(libc::SYS_personality, 0x40_0000) }),
            Some(libc::EPERM)
        );
    }
}
