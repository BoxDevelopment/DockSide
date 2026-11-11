//! Stack samples of DockSide's busy threads, for `cpu_watch`: what code each
//! thread is running, named by the Windows component it's in (and DockSide's
//! own code as offsets into dockside.exe, matched to its .pdb afterwards).
//!
//! A thread is paused only long enough to read its registers and copy its
//! stack (no locks taken and nothing allocated meanwhile), and is walked from
//! the copy once it runs again, so pausing a thread that holds the heap or
//! loader lock can't hang the sampler.

use std::collections::HashMap;
use std::ffi::c_void;
use std::time::Duration;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::Debug::{
    GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind, CONTEXT, CONTEXT_CONTROL_AMD64, CONTEXT_INTEGER_AMD64, UNW_FLAG_NHANDLER,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// Snapshots per thread, and the time between them.
const ROUNDS: usize = 20;
const GAP: Duration = Duration::from_millis(10);
/// Frames walked per stack.
const MAX_FRAMES: usize = 64;
/// Bytes of stack copied (from the stack pointer up); deeper frames are left out.
const STACK_COPY: usize = 256 * 1024;
/// Zeroed room after the copy, so unwinding the last frames never reads past the buffer.
const PAD: usize = 64 * 1024;
/// At most this many threads are sampled at once.
const MAX_THREADS: usize = 24;

/// Modules every thread passes through, which say nothing about what it's doing.
const GENERIC: [&str; 7] = ["ntdll.dll", "kernelbase.dll", "kernel32.dll", "ucrtbase.dll", "msvcrt.dll", "vcruntime140.dll", "win32u.dll"];

/// A thread to sample, and what kind it is (as `cpu_watch` names it).
pub struct Target {
    pub tid: u32,
    pub kind: String,
}

/// The link time stamp of this dockside.exe: which build's .pdb its offsets belong to.
pub fn build_stamp() -> u32 {
    unsafe {
        let Ok(exe) = GetModuleHandleW(PCWSTR::null()) else { return 0 };
        let base = exe.0 as usize;
        let nt = base + std::ptr::read_unaligned((base + 0x3C) as *const i32) as usize;
        std::ptr::read_unaligned((nt + 8) as *const u32)
    }
}

/// Samples `targets` for about `ROUNDS * GAP` and sums up what they ran.
pub fn sample(targets: &[Target]) -> String {
    let me = unsafe { GetCurrentThreadId() };
    let threads: Vec<(HANDLE, u64, &str)> = targets
        .iter()
        .filter(|t| t.tid != me)
        .take(MAX_THREADS)
        .filter_map(|t| unsafe {
            let h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION, false, t.tid).ok()?;
            match stack_base(h) {
                Some(base) => Some((h, base, t.kind.as_str())),
                None => {
                    let _ = CloseHandle(h);
                    None
                }
            }
        })
        .collect();
    let mut buf = vec![0u8; STACK_COPY + PAD];
    let mut stacks: Vec<(&str, Vec<u64>)> = Vec::new();
    for round in 0..ROUNDS {
        if round > 0 {
            std::thread::sleep(GAP);
        }
        for &(h, base, kind) in &threads {
            if let Some(stack) = unsafe { snapshot(h, base, &mut buf) } {
                stacks.push((kind, stack));
            }
        }
    }
    for (h, _, _) in &threads {
        unsafe {
            let _ = CloseHandle(*h);
        }
    }
    summarize(&stacks, threads.len())
}

/// The top of a thread's stack (its TEB's `StackBase`).
unsafe fn stack_base(h: HANDLE) -> Option<u64> {
    type Query = unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;
    const THREAD_BASIC_INFORMATION: u32 = 0;
    let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
    let query: Query = std::mem::transmute(GetProcAddress(ntdll, s!("NtQueryInformationThread"))?);
    // ExitStatus, TebBaseAddress, ClientId (2), AffinityMask, Priority + BasePriority.
    let mut info = [0usize; 6];
    if query(h, THREAD_BASIC_INFORMATION, info.as_mut_ptr() as *mut c_void, std::mem::size_of_val(&info) as u32, std::ptr::null_mut()) != 0 {
        return None;
    }
    let teb = info[1];
    (teb != 0).then(|| std::ptr::read((teb + 8) as *const u64))
}

/// Pauses the thread, copies its registers and stack into `buf`, lets it go,
/// then walks the copy. Return addresses, innermost first.
unsafe fn snapshot(h: HANDLE, base: u64, buf: &mut [u8]) -> Option<Vec<u64>> {
    if SuspendThread(h) == u32::MAX {
        return None;
    }
    // Nothing here may allocate or take a lock: the paused thread might hold it.
    let mut ctx = CONTEXT { ContextFlags: CONTEXT_CONTROL_AMD64 | CONTEXT_INTEGER_AMD64, ..Default::default() };
    let mut len = 0usize;
    if GetThreadContext(h, &mut ctx).is_ok() && ctx.Rsp < base {
        len = ((base - ctx.Rsp) as usize).min(STACK_COPY);
        std::ptr::copy_nonoverlapping(ctx.Rsp as *const u8, buf.as_mut_ptr(), len);
    }
    ResumeThread(h);
    (len > 0).then(|| walk(ctx, &mut buf[..len + PAD], len))
}

/// Unwinds `ctx` over the stack copied into `copy` (its first `len` bytes),
/// moving pointers into the real stack over to the copy first.
unsafe fn walk(mut ctx: CONTEXT, copy: &mut [u8], len: usize) -> Vec<u64> {
    let (lo, hi) = (ctx.Rsp, ctx.Rsp + len as u64);
    let at = copy.as_mut_ptr() as u64;
    let shift = |v: u64| if (lo..hi).contains(&v) { v - lo + at } else { v };
    for word in copy[..len].chunks_exact_mut(8) {
        let v = u64::from_le_bytes(word.try_into().unwrap());
        word.copy_from_slice(&shift(v).to_le_bytes());
    }
    for r in [&mut ctx.Rsp, &mut ctx.Rbp, &mut ctx.Rbx, &mut ctx.Rsi, &mut ctx.Rdi, &mut ctx.R12, &mut ctx.R13, &mut ctx.R14, &mut ctx.R15] {
        *r = shift(*r);
    }
    let end = at + len as u64;
    let mut frames = Vec::with_capacity(16);
    for _ in 0..MAX_FRAMES {
        let pc = ctx.Rip;
        if pc == 0 {
            break;
        }
        frames.push(pc);
        if !(at..end).contains(&ctx.Rsp) {
            break;
        }
        let mut image = 0u64;
        let f = RtlLookupFunctionEntry(pc, &mut image, None);
        if f.is_null() {
            // A leaf function: the return address is on top of the stack.
            ctx.Rip = std::ptr::read_unaligned(ctx.Rsp as *const u64);
            ctx.Rsp += 8;
        } else {
            let (mut data, mut frame) = (std::ptr::null_mut(), 0u64);
            RtlVirtualUnwind(UNW_FLAG_NHANDLER, image, pc, f, &mut ctx, &mut data, &mut frame, None);
        }
    }
    frames
}

/// A loaded module: where it is, its file name and its exports by address.
struct Module {
    base: u64,
    end: u64,
    name: String,
    exports: Vec<(u32, String)>,
}

#[derive(Default)]
struct Modules(Vec<Module>);

impl Modules {
    fn find(&mut self, addr: u64) -> Option<&Module> {
        if let Some(i) = self.0.iter().position(|m| (m.base..m.end).contains(&addr)) {
            return Some(&self.0[i]);
        }
        unsafe {
            let mut h = HMODULE::default();
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(addr as *const u16),
                &mut h,
            )
            .ok()?;
            let base = h.0 as usize;
            let nt = base + std::ptr::read_unaligned((base + 0x3C) as *const i32) as usize;
            let size = std::ptr::read_unaligned((nt + 24 + 56) as *const u32) as u64;
            let mut buf = [0u16; 260];
            let n = GetModuleFileNameW(h, &mut buf) as usize;
            let name = String::from_utf16_lossy(&buf[..n]).rsplit('\\').next().unwrap_or("?").to_lowercase();
            let exports = if name.ends_with(".exe") { Vec::new() } else { exports(base, nt) };
            self.0.push(Module { base: base as u64, end: base as u64 + size, name, exports });
            self.0.last()
        }
    }

    /// "module!Export+0x1f" (or "module+0x1234" where no export comes before it).
    fn label(&mut self, addr: u64) -> String {
        let Some(m) = self.find(addr) else { return format!("{addr:#x}") };
        let rva = (addr - m.base) as u32;
        let i = m.exports.partition_point(|(r, _)| *r <= rva);
        match i.checked_sub(1).map(|i| &m.exports[i]) {
            Some((r, name)) => format!("{}!{name}+{:#x}", m.name, rva - r),
            None => format!("{}+{rva:#x}", m.name),
        }
    }

    fn name(&mut self, addr: u64) -> String {
        self.find(addr).map_or_else(|| "?".into(), |m| m.name.clone())
    }
}

/// A PE image's named exports (address, name), by address.
unsafe fn exports(base: usize, nt: usize) -> Vec<(u32, String)> {
    let read32 = |a: usize| std::ptr::read_unaligned(a as *const u32);
    // The export entry of the PE32+ optional header's data directory.
    let dir = read32(nt + 24 + 112) as usize;
    let dir_size = read32(nt + 24 + 116) as usize;
    if dir == 0 {
        return Vec::new();
    }
    let ed = base + dir;
    let (names, funcs, ords) = (read32(ed + 32) as usize, read32(ed + 28) as usize, read32(ed + 36) as usize);
    let mut out = Vec::new();
    for i in 0..read32(ed + 24) as usize {
        let name_rva = read32(base + names + 4 * i) as usize;
        let ord = std::ptr::read_unaligned((base + ords + 2 * i) as *const u16) as usize;
        let rva = read32(base + funcs + 4 * ord);
        // Forwarders point back into the export directory, not at code.
        if (dir..dir + dir_size).contains(&(rva as usize)) {
            continue;
        }
        let name = std::ffi::CStr::from_ptr((base + name_rva) as *const std::ffi::c_char).to_string_lossy().into_owned();
        out.push((rva, name));
    }
    out.sort_unstable_by_key(|e| e.0);
    out
}

/// Whether a stack is a thread waiting for work rather than running: its
/// innermost frame is one of the kernel's wait calls.
fn idle(leaf: &str) -> bool {
    let Some((module, func)) = leaf.split_once('!') else { return false };
    (module == "ntdll.dll" || module == "win32u.dll")
        && (func.starts_with("Nt") || func.starts_with("Zw"))
        && ["Wait", "Remove", "Delay", "GetMessage", "WorkerFactory", "ReplyWaitReceive"].iter().any(|w| func.contains(w))
}

fn summarize(stacks: &[(&str, Vec<u64>)], threads: usize) -> String {
    let mut modules = Modules::default();
    let mut idle_n = 0;
    // Where each running stack was (its kind and first telling frame), how
    // often, and one example of the modules it went through.
    let mut at: HashMap<String, (u32, String)> = HashMap::new();
    let mut ours: HashMap<String, u32> = HashMap::new();
    for (kind, frames) in stacks {
        let Some(&leaf) = frames.first() else { continue };
        if idle(&modules.label(leaf)) {
            idle_n += 1;
            continue;
        }
        let names: Vec<String> = frames.iter().map(|&a| modules.name(a)).collect();
        let telling = names.iter().position(|n| !GENERIC.contains(&n.as_str())).unwrap_or(0);
        let key = format!("{kind}: {}", modules.label(frames[telling]));
        let e = at.entry(key).or_insert_with(|| {
            let mut chain: Vec<&str> = Vec::new();
            for n in &names {
                if chain.last() != Some(&n.as_str()) {
                    chain.push(n);
                }
            }
            (0, chain.into_iter().take(8).collect::<Vec<_>>().join(" < "))
        });
        e.0 += 1;
        if let Some(i) = names.iter().position(|n| n == "dockside.exe") {
            let a = frames[i];
            let rva = modules.find(a).map_or(0, |m| a - m.base);
            *ours.entry(format!("dockside.exe+{rva:#x}")).or_default() += 1;
        }
    }
    let running = stacks.len() - idle_n;
    let mut top: Vec<_> = at.into_iter().collect();
    top.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let top = top
        .iter()
        .take(6)
        .enumerate()
        .map(|(i, (k, (n, chain)))| if i < 3 { format!("{k} x{n} [{chain}]") } else { format!("{k} x{n}") })
        .collect::<Vec<_>>()
        .join("; ");
    let mut ours: Vec<_> = ours.into_iter().collect();
    ours.sort_by(|a, b| b.1.cmp(&a.1));
    let ours = ours.iter().take(6).map(|(k, n)| format!("{k} x{n}")).collect::<Vec<_>>().join(", ");
    let mut line = format!("cpu stacks: {running} of {} snapshots running, {threads} threads; {top}", stacks.len());
    if !ours.is_empty() {
        line += &format!("; DockSide code: {ours}");
    }
    line
}
