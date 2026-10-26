//! What `exec` opens beyond the program it names. The sandbox judges each of
//! them as it judges the program, so a child may run a program and still be
//! refused the interpreter the program needs.

use std::ffi::OsString;
use std::fs::File;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Linux's `BINPRM_BUF_SIZE`: how much of a `#!` line the kernel reads.
const SHEBANG_MAX: usize = 256;
/// How many `#!` interpreters in turn the kernel follows before `ELOOP`.
const SCRIPTS_MAX: usize = 5;
const PT_INTERP: u64 = 3;

/// The files `exec` of `program` opens after it, in order and at their real
/// paths: each `#!` interpreter, then the ELF loader the last one names. The
/// list ends where the host cannot read or resolve the next file.
pub(super) fn interpreters(program: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut current = program.to_owned();
    while chain.len() < SCRIPTS_MAX {
        let Ok(file) = File::open(&current) else {
            break;
        };
        let Some(next) = shebang(&file).or_else(|| elf_interpreter(&file)) else {
            break;
        };
        let Ok(next) = std::fs::canonicalize(next) else {
            break;
        };
        chain.push(next.clone());
        current = next;
    }
    chain
}

/// The interpreter a `#!` line names, where it names one by absolute path:
/// a relative one resolves against the caller's directory, which the host
/// does not share.
fn shebang(file: &File) -> Option<PathBuf> {
    let mut head = [0u8; SHEBANG_MAX];
    let read = file.read_at(&mut head, 0).ok()?;
    let line = head[..read].strip_prefix(b"#!")?;
    let line = &line[..line
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(line.len())];
    let interpreter = line
        .split(|&byte| byte == b' ' || byte == b'\t')
        .find(|word| !word.is_empty())?;
    let interpreter = PathBuf::from(OsString::from_vec(interpreter.to_vec()));
    interpreter.is_absolute().then_some(interpreter)
}

/// The loader an ELF file's `PT_INTERP` names.
fn elf_interpreter(file: &File) -> Option<PathBuf> {
    let mut header = [0u8; 64];
    file.read_exact_at(&mut header, 0).ok()?;
    if header[..4] != *b"\x7fELF" {
        return None;
    }
    let wide = match header[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let little = match header[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    let word = |bytes: &[u8], at: usize, len: usize| {
        let field = &bytes[at..at + len];
        let fold = |acc: u64, byte: &u8| acc << 8 | u64::from(*byte);
        if little {
            field.iter().rev().fold(0, fold)
        } else {
            field.iter().fold(0, fold)
        }
    };
    let (phoff, phentsize, phnum, entry_min) = if wide {
        (
            word(&header, 0x20, 8),
            word(&header, 0x36, 2),
            word(&header, 0x38, 2),
            0x38,
        )
    } else {
        (
            word(&header, 0x1c, 4),
            word(&header, 0x2a, 2),
            word(&header, 0x2c, 2),
            0x20,
        )
    };
    let mut entry = vec![0u8; usize::try_from(phentsize).ok()?];
    if entry.len() < entry_min {
        return None;
    }
    for index in 0..phnum {
        file.read_exact_at(&mut entry, phoff.checked_add(index * phentsize)?)
            .ok()?;
        if word(&entry, 0, 4) != PT_INTERP {
            continue;
        }
        let (offset, size) = if wide {
            (word(&entry, 0x08, 8), word(&entry, 0x20, 8))
        } else {
            (word(&entry, 0x04, 4), word(&entry, 0x10, 4))
        };
        if size > libc::PATH_MAX as u64 {
            return None;
        }
        let mut name = vec![0u8; usize::try_from(size).ok()?];
        file.read_exact_at(&mut name, offset).ok()?;
        name.truncate(name.iter().position(|&byte| byte == 0)?);
        return Some(PathBuf::from(OsString::from_vec(name)));
    }
    None
}
