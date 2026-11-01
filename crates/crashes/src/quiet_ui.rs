//! quiet-ui: what a session that ended without quitting left behind, said in
//! the log of the next one. A session records itself in a marker file the
//! moment it starts and removes it when it quits, so a marker found at launch
//! is a session that crashed, was killed or lost power.

use crate::CrashInfo;
use minidump_common::format::{
    CONTEXT_AMD64, CONTEXT_ARM64, CONTEXT_ARM64_OLD, MINIDUMP_DIRECTORY, MINIDUMP_EXCEPTION_STREAM,
    MINIDUMP_HEADER, MINIDUMP_MODULE, MINIDUMP_SIGNATURE, MINIDUMP_STREAM_TYPE,
    MINIDUMP_SYSTEM_INFO, MINIDUMP_THREAD, ProcessorArchitecture,
};
use scroll::{LE, Pread as _, ctx::SizeWith as _};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fmt::Write as _,
    fs,
    io::{Read as _, Seek as _, SeekFrom},
    path::{Path, PathBuf},
};

const MARKER: &str = "quiet-ui-session.json";
/// Minidumps kept beside the log; there is no endpoint to upload them to.
pub const MINIDUMPS_KEPT: usize = 10;
const FRAMES_SHOWN: usize = 24;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionMarker {
    pub session_id: String,
    pub version: String,
    pub started_at: String,
}

/// The session the marker in `logs_dir` names, if the last one did not quit,
/// with the last line it wrote to `log_file`. Read before this session logs
/// anything, so the log's last line is still the previous session's.
pub fn unfinished_session(
    logs_dir: &Path,
    log_file: &Path,
) -> Option<(SessionMarker, Option<String>)> {
    let marker = fs::read(logs_dir.join(MARKER)).ok()?;
    let marker = serde_json::from_slice(&marker).ok()?;
    Some((marker, last_log_line(log_file)))
}

pub fn record_session(logs_dir: &Path, marker: &SessionMarker) {
    if let Ok(marker) = serde_json::to_vec(marker) {
        fs::write(logs_dir.join(MARKER), marker).ok();
    }
}

pub fn end_session(logs_dir: &Path) {
    fs::remove_file(logs_dir.join(MARKER)).ok();
}

/// What to log about a session that ended without quitting: when it started
/// and last logged, and what its minidump says, if it left one.
/// `current_commit` decides whether the dump's frames can be named from this
/// binary.
pub fn report(
    logs_dir: &Path,
    previous: &SessionMarker,
    last_line: Option<&str>,
    current_commit: Option<&str>,
) -> String {
    let mut report = format!(
        "quiet-ui: the previous session (version {}, started at {}",
        previous.version, previous.started_at
    );
    match last_line {
        Some(line) => {
            let at = line.split_whitespace().next().unwrap_or_default();
            let line: String = line.chars().take(300).collect();
            write!(
                report,
                ", last log line at {at}) ended without quitting\n  last line: {line}"
            )
            .ok();
        }
        None => report.push_str(") ended without quitting"),
    }

    let dump_path = logs_dir.join(&previous.session_id).with_extension("dmp");
    if !dump_path.exists() {
        report.push_str("\n  it left no minidump");
        return report;
    }
    write!(report, "\n  minidump: {}", dump_path.display()).ok();
    let info = fs::read(dump_path.with_extension("json"))
        .ok()
        .and_then(|info| serde_json::from_slice::<CrashInfo>(&info).ok());
    if let Some(panic) = info.as_ref().and_then(|info| info.panic.as_ref()) {
        write!(report, "\n  panic at {}: {}", panic.span, panic.message).ok();
    }
    if let Some(abort) = info.as_ref().and_then(|info| info.abort_message.as_ref()) {
        write!(report, "\n  abort message: {abort}").ok();
    }
    let crash = fs::read(&dump_path)
        .map_err(anyhow::Error::from)
        .and_then(|dump| decompress(&dump))
        .and_then(|dump| crashing_thread(&dump));
    match crash {
        Ok(crash) => {
            let same_binary = current_commit.is_some()
                && info.as_ref().map(|info| info.init.commit_sha.as_str()) == current_commit;
            write!(report, "\n  crashing thread {}:", crash.thread_id).ok();
            for (ix, address) in crash.frames.iter().enumerate() {
                write!(
                    report,
                    "\n    {ix:>2} {}",
                    describe_address(*address, &crash.modules, same_binary)
                )
                .ok();
            }
            if !same_binary {
                report.push_str(
                    "\n  (the dump is from another build, so only module offsets are given)",
                );
            }
        }
        Err(error) => {
            write!(
                report,
                "\n  its crashing thread could not be read: {error:#}"
            )
            .ok();
        }
    }
    report
}

/// Deletes all but the newest `keep` minidumps in `logs_dir`, with the
/// `.json` beside each.
pub fn prune_minidumps(logs_dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(logs_dir) else {
        return;
    };
    let mut dumps = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("dmp")))
        .filter_map(|path| Some((fs::metadata(&path).ok()?.modified().ok()?, path)))
        .collect::<Vec<_>>();
    dumps.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in dumps.into_iter().skip(keep) {
        fs::remove_file(path.with_extension("json")).ok();
        fs::remove_file(path).ok();
    }
}

fn last_log_line(log_file: &Path) -> Option<String> {
    const TAIL: u64 = 64 * 1024;
    let mut file = fs::File::open(log_file).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
}

/// The crash server compresses each dump once written.
fn decompress(dump: &[u8]) -> anyhow::Result<Vec<u8>> {
    if dump.starts_with(&MINIDUMP_SIGNATURE.to_le_bytes()) {
        return Ok(dump.to_vec());
    }
    Ok(zstd::stream::decode_all(dump)?)
}

#[derive(Debug, PartialEq)]
struct Module {
    base: u64,
    size: u64,
    name: String,
}

#[derive(Debug, PartialEq)]
struct CrashingThread {
    thread_id: u32,
    modules: Vec<Module>,
    /// The program counter, then return addresses found by following the
    /// frame pointers through the thread's stack.
    frames: Vec<u64>,
}

fn crashing_thread(dump: &[u8]) -> anyhow::Result<CrashingThread> {
    let header: MINIDUMP_HEADER = dump.pread_with(0, LE)?;
    anyhow::ensure!(header.signature == MINIDUMP_SIGNATURE, "not a minidump");
    let directory_size = MINIDUMP_DIRECTORY::size_with(&LE);
    let streams = (0..header.stream_count as usize)
        .map(|ix| {
            dump.pread_with::<MINIDUMP_DIRECTORY>(
                header.stream_directory_rva as usize + ix * directory_size,
                LE,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stream = |kind: MINIDUMP_STREAM_TYPE| {
        streams
            .iter()
            .find(|stream| stream.stream_type == kind as u32)
            .map(|stream| stream.location.rva as usize)
    };

    let exception: MINIDUMP_EXCEPTION_STREAM = dump.pread_with(
        stream(MINIDUMP_STREAM_TYPE::ExceptionStream)
            .ok_or_else(|| anyhow::anyhow!("the dump records no exception"))?,
        LE,
    )?;
    let architecture = match stream(MINIDUMP_STREAM_TYPE::SystemInfoStream) {
        Some(at) => {
            dump.pread_with::<MINIDUMP_SYSTEM_INFO>(at, LE)?
                .processor_architecture
        }
        None => anyhow::bail!("the dump records no system info"),
    };
    let context = exception.thread_context.rva as usize;
    let (pc, link, frame_pointer) = match architecture {
        arch if arch == ProcessorArchitecture::PROCESSOR_ARCHITECTURE_AMD64 as u16 => {
            let context: CONTEXT_AMD64 = dump.pread_with(context, LE)?;
            (context.rip, None, context.rbp)
        }
        arch if arch == ProcessorArchitecture::PROCESSOR_ARCHITECTURE_ARM64_OLD as u16 => {
            let context: CONTEXT_ARM64_OLD = dump.pread_with(context, LE)?;
            (context.pc, Some(context.iregs[30]), context.iregs[29])
        }
        arch if arch == ProcessorArchitecture::PROCESSOR_ARCHITECTURE_ARM64 as u16 => {
            let context: CONTEXT_ARM64 = dump.pread_with(context, LE)?;
            (context.pc, Some(context.iregs[30]), context.iregs[29])
        }
        arch => anyhow::bail!("unsupported processor architecture {arch}"),
    };

    let mut frames = vec![pc];
    frames.extend(link.filter(|link| *link != 0));
    if let Some(at) = stream(MINIDUMP_STREAM_TYPE::ThreadListStream) {
        let count: u32 = dump.pread_with(at, LE)?;
        let thread_size = MINIDUMP_THREAD::size_with(&LE);
        for ix in 0..count as usize {
            let thread: MINIDUMP_THREAD = dump.pread_with(at + 4 + ix * thread_size, LE)?;
            if thread.thread_id != exception.thread_id {
                continue;
            }
            let start = thread.stack.start_of_memory_range;
            let memory = thread.stack.memory;
            let stack = dump
                .get(memory.rva as usize..memory.rva as usize + memory.data_size as usize)
                .unwrap_or_default();
            let read = |address: u64| -> Option<u64> {
                let offset = address.checked_sub(start)? as usize;
                stack.pread_with::<u64>(offset, LE).ok()
            };
            let mut frame = frame_pointer;
            while frames.len() < FRAMES_SHOWN {
                let (Some(next), Some(ret)) = (read(frame), read(frame + 8)) else {
                    break;
                };
                if ret == 0 {
                    break;
                }
                // A return address that repeats the link register is the
                // frame the link register already named.
                if frames.len() != 2 || link != Some(ret) {
                    frames.push(ret);
                }
                if next <= frame {
                    break;
                }
                frame = next;
            }
        }
    }

    let mut modules = Vec::new();
    if let Some(at) = stream(MINIDUMP_STREAM_TYPE::ModuleListStream) {
        let count: u32 = dump.pread_with(at, LE)?;
        let module_size = MINIDUMP_MODULE::size_with(&LE);
        for ix in 0..count as usize {
            let module: MINIDUMP_MODULE = dump.pread_with(at + 4 + ix * module_size, LE)?;
            modules.push(Module {
                base: module.base_of_image,
                size: module.size_of_image as u64,
                name: read_string(dump, module.module_name_rva as usize).unwrap_or_default(),
            });
        }
    }

    Ok(CrashingThread {
        thread_id: exception.thread_id,
        modules,
        frames,
    })
}

/// A `MINIDUMP_STRING`: a byte length, then UTF-16LE.
fn read_string(dump: &[u8], at: usize) -> Option<String> {
    let len: u32 = dump.pread_with(at, LE).ok()?;
    let bytes = dump.get(at + 4..at + 4 + len as usize)?;
    let units = bytes
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .collect::<Vec<_>>();
    Some(String::from_utf16_lossy(&units))
}

fn describe_address(address: u64, modules: &[Module], same_binary: bool) -> String {
    let Some(module) = modules
        .iter()
        .find(|module| (module.base..module.base + module.size).contains(&address))
    else {
        return format!("{address:#x}");
    };
    let offset = address - module.base;
    let file_name = Path::new(&module.name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| module.name.clone());
    let mut description = format!("{file_name}+{offset:#x}");
    if same_binary
        && is_this_executable(&module.name)
        && let Some(symbol) = symbol_at_offset(offset)
    {
        write!(description, " {symbol}").ok();
    }
    description
}

fn is_this_executable(module_name: &str) -> bool {
    let module = PathBuf::from(module_name);
    std::env::current_exe()
        .is_ok_and(|exe| exe == module || exe.canonicalize().ok() == module.canonicalize().ok())
}

/// Names the code at `offset` into this executable, which is the binary the
/// dump came from when the commits match.
#[cfg(unix)]
fn symbol_at_offset(offset: u64) -> Option<String> {
    let base = {
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // SAFETY: dladdr only reads the address it is given and fills `info`.
        let found = unsafe { libc::dladdr(symbol_at_offset as *const libc::c_void, &mut info) };
        if found == 0 || info.dli_fbase.is_null() {
            return None;
        }
        info.dli_fbase as u64
    };
    // A return address points just after its call; look the call up.
    let address = (base + offset).saturating_sub(1);
    let mut name = None;
    backtrace::resolve(address as *mut std::ffi::c_void, |symbol| {
        if name.is_some() {
            return;
        }
        let mut described = symbol
            .name()
            .map(|name| format!("{name:#}"))
            .unwrap_or_else(|| "?".into());
        if let (Some(file), Some(line)) = (symbol.filename(), symbol.lineno()) {
            write!(described, " ({}:{line})", file.display()).ok();
        }
        name = Some(described);
    });
    name
}

#[cfg(not(unix))]
fn symbol_at_offset(_offset: u64) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use minidump_common::format::{
        MINIDUMP_EXCEPTION, MINIDUMP_LOCATION_DESCRIPTOR, MINIDUMP_MEMORY_DESCRIPTOR,
    };
    use scroll::Pwrite as _;

    #[test]
    fn test_a_session_that_did_not_quit_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("Zed.log");
        fs::write(
            &log,
            "2026-10-22T10:50:59+02:00 INFO  [git_ui] refreshed\n\
             2026-10-22T10:51:05+02:00 WARN  [git_ui::diff_multibuffer] 286ms\n\n",
        )
        .unwrap();
        assert_eq!(unfinished_session(dir.path(), &log), None, "a first launch");

        let first = SessionMarker {
            session_id: "first".into(),
            version: "0.231.0+dev.e31126a".into(),
            started_at: "2026-10-22T09:12:00+02:00".into(),
        };
        record_session(dir.path(), &first);
        let (previous, last_line) = unfinished_session(dir.path(), &log).unwrap();
        assert_eq!(previous, first);
        let report = report(dir.path(), &previous, last_line.as_deref(), Some("abc"));
        assert!(
            report.starts_with(
                "quiet-ui: the previous session (version 0.231.0+dev.e31126a, started at \
                 2026-10-22T09:12:00+02:00, last log line at 2026-10-22T10:51:05+02:00) ended \
                 without quitting"
            ),
            "{report}"
        );
        assert!(report.ends_with("it left no minidump"), "{report}");

        end_session(dir.path());
        assert_eq!(unfinished_session(dir.path(), &log), None, "it quit");
    }

    #[test]
    fn test_only_the_newest_minidumps_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        for ix in 0..12 {
            let dump = dir.path().join(format!("{ix}.dmp"));
            fs::write(&dump, b"").unwrap();
            fs::write(dump.with_extension("json"), b"{}").unwrap();
            let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(ix);
            fs::File::options()
                .write(true)
                .open(&dump)
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
        prune_minidumps(dir.path(), MINIDUMPS_KEPT);
        let mut left = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        left.sort();
        let mut expected = (2..12)
            .flat_map(|ix| [format!("{ix}.dmp"), format!("{ix}.json")])
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(left, expected);
    }

    #[cfg(unix)]
    #[test]
    fn test_a_frame_in_this_binary_is_named() {
        let function = describe_address as *const libc::c_void;
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        assert_ne!(unsafe { libc::dladdr(function, &mut info) }, 0);
        let base = info.dli_fbase as u64;
        let modules = [Module {
            base,
            size: function as u64 - base + 0x1000,
            name: std::env::current_exe().unwrap().display().to_string(),
        }];
        let described = describe_address(function as u64 + 4, &modules, true);
        assert!(described.contains("describe_address"), "{described}");
        assert!(
            !describe_address(function as u64 + 4, &modules, false).contains("describe_address"),
            "a dump from another build is not named from this one"
        );
    }

    /// A dump shaped as minidump-writer writes one on an Apple Silicon Mac:
    /// the crashing thread's context, its stack, and the module list.
    #[test]
    fn test_the_crashing_thread_is_walked_through_its_frame_pointers() {
        const ZED: u64 = 0x1_0000_0000;
        const STACK: u64 = 0x16f0_0000;
        let mut dump = vec![0u8; 0x4000];
        let directory = 0x100;
        let streams = [
            (MINIDUMP_STREAM_TYPE::ExceptionStream, 0x200),
            (MINIDUMP_STREAM_TYPE::SystemInfoStream, 0x400),
            (MINIDUMP_STREAM_TYPE::ThreadListStream, 0x600),
            (MINIDUMP_STREAM_TYPE::ModuleListStream, 0x800),
        ];
        let context_at = 0x1000;
        let stack_at = 0x2000;
        let name_at = 0x3000;
        dump.pwrite_with(
            MINIDUMP_HEADER {
                signature: MINIDUMP_SIGNATURE,
                version: 0xa793,
                stream_count: streams.len() as u32,
                stream_directory_rva: directory,
                checksum: 0,
                time_date_stamp: 0,
                flags: 0,
            },
            0,
            LE,
        )
        .unwrap();
        for (ix, (kind, at)) in streams.iter().enumerate() {
            dump.pwrite_with(
                MINIDUMP_DIRECTORY {
                    stream_type: *kind as u32,
                    location: MINIDUMP_LOCATION_DESCRIPTOR {
                        data_size: 0x100,
                        rva: *at,
                    },
                },
                directory as usize + ix * MINIDUMP_DIRECTORY::size_with(&LE),
                LE,
            )
            .unwrap();
        }
        let context_location = MINIDUMP_LOCATION_DESCRIPTOR {
            data_size: CONTEXT_ARM64_OLD::size_with(&LE) as u32,
            rva: context_at,
        };
        let mut exception = MINIDUMP_EXCEPTION_STREAM {
            thread_id: 7,
            __align: 0,
            exception_record: MINIDUMP_EXCEPTION::default(),
            thread_context: context_location,
        };
        exception.exception_record.exception_code = 6;
        dump.pwrite_with(exception, 0x200, LE).unwrap();
        let mut system_info: MINIDUMP_SYSTEM_INFO = dump.pread_with(0x400, LE).unwrap();
        system_info.processor_architecture =
            ProcessorArchitecture::PROCESSOR_ARCHITECTURE_ARM64_OLD as u16;
        dump.pwrite_with(system_info, 0x400, LE).unwrap();

        let mut context: CONTEXT_ARM64_OLD = dump.pread_with(context_at as usize, LE).unwrap();
        context.pc = ZED + 0x100;
        context.iregs[30] = ZED + 0x200;
        context.iregs[29] = STACK + 0x10;
        dump.pwrite_with(context, context_at as usize, LE).unwrap();
        // Two frame records, the second ending the chain.
        let stack = [
            (0x10, STACK + 0x40),
            (0x18, ZED + 0x200),
            (0x40, STACK + 0x80),
            (0x48, ZED + 0x300),
            (0x80, 0),
            (0x88, ZED + 0x400),
        ];
        for (offset, value) in stack {
            dump.pwrite_with(value, stack_at + offset, LE).unwrap();
        }

        dump.pwrite_with(2u32, 0x600, LE).unwrap();
        let thread_size = MINIDUMP_THREAD::size_with(&LE);
        for (ix, thread_id) in [3u32, 7].into_iter().enumerate() {
            let mut thread: MINIDUMP_THREAD = dump.pread_with(0x604, LE).unwrap();
            thread.thread_id = thread_id;
            thread.stack = MINIDUMP_MEMORY_DESCRIPTOR {
                start_of_memory_range: STACK,
                memory: MINIDUMP_LOCATION_DESCRIPTOR {
                    data_size: 0x100,
                    rva: stack_at as u32,
                },
            };
            thread.thread_context = context_location;
            dump.pwrite_with(thread, 0x604 + ix * thread_size, LE)
                .unwrap();
        }

        dump.pwrite_with(1u32, 0x800, LE).unwrap();
        let mut module: MINIDUMP_MODULE = dump.pread_with(0x804, LE).unwrap();
        module.base_of_image = ZED;
        module.size_of_image = 0x1000;
        module.module_name_rva = name_at;
        dump.pwrite_with(module, 0x804, LE).unwrap();
        let name = "/Applications/Zed Dev.app/Contents/MacOS/zed"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        dump.pwrite_with(name.len() as u32, name_at as usize, LE)
            .unwrap();
        dump[name_at as usize + 4..][..name.len()].copy_from_slice(&name);

        let crash = crashing_thread(&dump).unwrap();
        assert_eq!(crash.thread_id, 7);
        assert_eq!(
            crash.frames,
            [ZED + 0x100, ZED + 0x200, ZED + 0x300, ZED + 0x400],
            "the frame the link register names is not repeated"
        );
        assert_eq!(
            describe_address(crash.frames[2], &crash.modules, false),
            "zed+0x300"
        );
        assert_eq!(describe_address(0x42, &crash.modules, false), "0x42");

        let compressed = zstd::stream::encode_all(dump.as_slice(), 0).unwrap();
        assert_eq!(decompress(&compressed).unwrap(), dump);
    }
}
