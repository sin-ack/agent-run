//! Seccomp filters used to harden the sandbox.

use std::{
    io::{PipeWriter, Write as _},
    os::fd::OwnedFd,
};

const LOAD_WORD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const JUMP_EQUAL: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const RETURN: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

const SYSCALL_OFFSET: u32 = std::mem::offset_of!(libc::seccomp_data, nr) as u32;
const ARCH_OFFSET: u32 = std::mem::offset_of!(libc::seccomp_data, arch) as u32;
const ARGUMENT_1_OFFSET: u32 =
    (std::mem::offset_of!(libc::seccomp_data, args) + std::mem::size_of::<u64>()) as u32;

const fn statement(code: u16, value: u32) -> libc::sock_filter {
    libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k: value,
    }
}

const fn jump(value: u32, jump_true: u8, jump_false: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: JUMP_EQUAL,
        jt: jump_true,
        jf: jump_false,
        k: value,
    }
}

#[cfg(target_arch = "x86_64")]
fn tiocsti_filter() -> [libc::sock_filter; 13] {
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    const AUDIT_ARCH_I386: u32 = 0x4000_0003;
    const X32_IOCTL: u32 = 0x4000_0202;
    const I386_IOCTL: u32 = 54;

    [
        statement(LOAD_WORD, ARCH_OFFSET),
        jump(AUDIT_ARCH_X86_64, 2, 0),
        jump(AUDIT_ARCH_I386, 4, 0),
        statement(RETURN, libc::SECCOMP_RET_KILL_PROCESS),
        statement(LOAD_WORD, SYSCALL_OFFSET),
        jump(libc::SYS_ioctl as u32, 3, 0),
        jump(X32_IOCTL, 2, 5),
        statement(LOAD_WORD, SYSCALL_OFFSET),
        jump(I386_IOCTL, 0, 3),
        statement(LOAD_WORD, ARGUMENT_1_OFFSET),
        jump(libc::TIOCSTI as u32, 0, 1),
        statement(RETURN, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
        statement(RETURN, libc::SECCOMP_RET_ALLOW),
    ]
}

#[cfg(target_arch = "aarch64")]
fn tiocsti_filter() -> [libc::sock_filter; 12] {
    const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
    const AUDIT_ARCH_ARM: u32 = 0x4000_0028;
    const ARM_IOCTL: u32 = 54;

    [
        statement(LOAD_WORD, ARCH_OFFSET),
        jump(AUDIT_ARCH_AARCH64, 2, 0),
        jump(AUDIT_ARCH_ARM, 3, 0),
        statement(RETURN, libc::SECCOMP_RET_KILL_PROCESS),
        statement(LOAD_WORD, SYSCALL_OFFSET),
        jump(libc::SYS_ioctl as u32, 2, 5),
        statement(LOAD_WORD, SYSCALL_OFFSET),
        jump(ARM_IOCTL, 0, 3),
        statement(LOAD_WORD, ARGUMENT_1_OFFSET),
        jump(libc::TIOCSTI as u32, 0, 1),
        statement(RETURN, libc::SECCOMP_RET_ERRNO | libc::EPERM as u32),
        statement(RETURN, libc::SECCOMP_RET_ALLOW),
    ]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("the seccomp filter only supports x86_64 and aarch64");

/// Create a file descriptor containing a seccomp filter that rejects TIOCSTI.
pub(crate) fn create_tiocsti_filter() -> std::io::Result<OwnedFd> {
    // Do not use O_CLOEXEC: bwrap needs to inherit the read end of the pipe.
    let (reader, writer) = nix::unistd::pipe().map_err(std::io::Error::from)?;
    let mut writer = PipeWriter::from(writer);

    for instruction in tiocsti_filter() {
        writer.write_all(&instruction.code.to_ne_bytes())?;
        writer.write_all(&[instruction.jt, instruction.jf])?;
        writer.write_all(&instruction.k.to_ne_bytes())?;
    }

    drop(writer);
    Ok(reader)
}
