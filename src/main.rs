// fastfind - native file/folder search engine (Everything-style), no Everything needed.
//
// Backends:
//   walk : parallel FindFirstFileExW traversal over rayon (no admin). Captures
//          size + last-write time per entry for free.
//   mft  : NTFS $MFT enumeration via FSCTL_ENUM_USN_DATA (needs Administrator).
//          Size/mtime are not in USN records, so they are stat'd lazily only for
//          the entries a size/date filter or the output actually needs.
//
// Structured filtering (no path-regex gymnastics): ext, any_of (OR), all_of (AND),
// none_of (NOT), size + mtime bounds, plus built-in ignore rules (node_modules,
// .git, caches, TinyTeX, Steam, hashed cache filenames, ...) and a user exclude
// list. Output is structured JSON: per-hit {path,name,dir,ext,size,mtime} plus a
// dirs:{path:count} rollup for trivial grouping.
//
// Modes: one-shot CLI, `--serve` (resident index, JSON-per-line stdin protocol),
// `--mcp` (Model Context Protocol server over stdio), and `install` (register the
// MCP server into detected agents).
#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals)]

use std::env;
use std::ffi::c_void;
use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use rustc_hash::FxHashMap;
use serde_json::{json, Map, Value};

// ------------------------------- Win32 FFI -------------------------------
type DWORD = u32;
type BOOL = i32;
type HANDLE = isize;

const INVALID_HANDLE_VALUE: HANDLE = -1;
const GENERIC_READ: DWORD = 0x8000_0000;
const FILE_SHARE_RW: DWORD = 0x0000_0003;
const OPEN_EXISTING: DWORD = 3;
const FILE_ATTRIBUTE_DIRECTORY: DWORD = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: DWORD = 0x400;
const ERROR_ACCESS_DENIED: DWORD = 5;
const ERROR_HANDLE_EOF: DWORD = 38;
const FindExInfoBasic: i32 = 1;
const FindExSearchNameMatch: i32 = 0;
const FIND_FIRST_EX_LARGE_FETCH: DWORD = 2;
const DRIVE_FIXED: u32 = 3;
const FSCTL_ENUM_USN_DATA: DWORD = 0x0009_00B3;
const FSCTL_QUERY_USN_JOURNAL: DWORD = 0x0009_00F4;
const MAX_USN: i64 = 0x7FFF_FFFF_FFFF_FFFF;

#[repr(C)]
struct FILETIME {
    low: DWORD,
    high: DWORD,
}

#[repr(C)]
struct WIN32_FIND_DATAW {
    dwFileAttributes: DWORD,
    ftCreationTime: FILETIME,
    ftLastAccessTime: FILETIME,
    ftLastWriteTime: FILETIME,
    nFileSizeHigh: DWORD,
    nFileSizeLow: DWORD,
    dwReserved0: DWORD,
    dwReserved1: DWORD,
    cFileName: [u16; 260],
    cAlternateFileName: [u16; 14],
}

extern "system" {
    fn GetLastError() -> DWORD;
    fn CloseHandle(h: HANDLE) -> BOOL;
    fn CreateFileW(name: *const u16, access: DWORD, share: DWORD, sec: *mut c_void,
                   disp: DWORD, flags: DWORD, templ: HANDLE) -> HANDLE;
    fn DeviceIoControl(h: HANDLE, code: DWORD, inbuf: *const c_void, insize: DWORD,
                       outbuf: *mut c_void, outsize: DWORD, ret: *mut DWORD,
                       ov: *mut c_void) -> BOOL;
    fn FindFirstFileExW(name: *const u16, level: i32, data: *mut WIN32_FIND_DATAW,
                        op: i32, filter: *mut c_void, flags: DWORD) -> HANDLE;
    fn FindNextFileW(h: HANDLE, data: *mut WIN32_FIND_DATAW) -> BOOL;
    fn FindClose(h: HANDLE) -> BOOL;
    fn GetLogicalDrives() -> DWORD;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetVolumeInformationW(root: *const u16, vol: *mut u16, volsz: DWORD, ser: *mut DWORD,
                             mcl: *mut DWORD, flags: *mut DWORD, fs: *mut u16, fssz: DWORD) -> BOOL;
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wstr(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// FILETIME (100ns ticks since 1601) -> unix seconds. 0 => unknown.
fn ft_to_unix(low: u32, high: u32) -> i64 {
    let ticks = ((high as u64) << 32) | (low as u64);
    if ticks == 0 {
        return 0;
    }
    (ticks / 10_000_000) as i64 - 11_644_473_600
}

// A raw index entry before finalization: (full_path, is_dir, size, mtime_unix).
type Raw = (String, bool, u64, i64);

// -------------------------- Volume enumeration ---------------------------
fn fs_type(drive: &str) -> String {
    let root = to_wide(&format!("{}\\", drive));
    let mut fsbuf = [0u16; 64];
    let mut volbuf = [0u16; 64];
    let (mut ser, mut mcl, mut fl) = (0u32, 0u32, 0u32);
    let ok = unsafe {
        GetVolumeInformationW(root.as_ptr(), volbuf.as_mut_ptr(), 64, &mut ser, &mut mcl,
                              &mut fl, fsbuf.as_mut_ptr(), 64)
    };
    if ok != 0 { wstr(&fsbuf) } else { String::new() }
}

fn drives_of_type(want_ntfs: bool) -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) != 0 {
            let drive = format!("{}:", (b'A' + i as u8) as char);
            let root = to_wide(&format!("{}\\", drive));
            if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_FIXED
                && (!want_ntfs || fs_type(&drive) == "NTFS")
            {
                out.push(drive);
            }
        }
    }
    out
}

fn ntfs_drives() -> Vec<String> {
    drives_of_type(true)
}

fn fixed_roots() -> Vec<String> {
    drives_of_type(false)
}

// ---------------------------- Walk backend -------------------------------
fn normalize_root(r: &str) -> String {
    r.replace('/', "\\").trim_end_matches('\\').to_string()
}

fn walk_dir(s: &rayon::Scope<'_>, dir: String, tx: Sender<Vec<Raw>>) {
    let search = to_wide(&format!("\\\\?\\{}\\*", dir));
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let h = unsafe {
        FindFirstFileExW(search.as_ptr(), FindExInfoBasic, &mut data,
                         FindExSearchNameMatch, std::ptr::null_mut(), FIND_FIRST_EX_LARGE_FETCH)
    };
    if h == INVALID_HANDLE_VALUE {
        return;
    }
    let mut local: Vec<Raw> = Vec::new();
    let mut subdirs = Vec::new();
    loop {
        let name = wstr(&data.cFileName);
        if name != "." && name != ".." {
            let is_dir = data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let is_reparse = data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            let full = format!("{}\\{}", dir, name);
            let size = ((data.nFileSizeHigh as u64) << 32) | (data.nFileSizeLow as u64);
            let mtime = ft_to_unix(data.ftLastWriteTime.low, data.ftLastWriteTime.high);
            if is_dir && !is_reparse {
                subdirs.push(full.clone());
            }
            local.push((full, is_dir, if is_dir { 0 } else { size }, mtime));
        }
        if unsafe { FindNextFileW(h, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(h) };
    let _ = tx.send(local);
    for sub in subdirs {
        let t = tx.clone();
        s.spawn(move |s| walk_dir(s, sub, t));
    }
}

fn build_walk(roots: &[String], threads: usize) -> Vec<Raw> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<Raw>>();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
    pool.scope(|s| {
        for r in roots {
            let dir = normalize_root(r);
            let t = tx.clone();
            s.spawn(move |s| walk_dir(s, dir, t));
        }
    });
    drop(tx);
    let mut all = Vec::new();
    for v in rx {
        all.extend(v);
    }
    all
}

// ----------------------------- MFT backend -------------------------------
fn rd_u16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn rd_u32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn rd_u64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

fn parse_usn(b: &[u8], nodes: &mut FxHashMap<u64, (String, u64, bool)>) {
    let n = b.len();
    let mut off = 8usize;
    while off + 60 <= n {
        let reclen = rd_u32(b, off) as usize;
        if reclen == 0 || off + reclen > n {
            break;
        }
        if rd_u16(b, off + 4) == 2 {
            let frn = rd_u64(b, off + 8);
            let parent = rd_u64(b, off + 16);
            let attrs = rd_u32(b, off + 52);
            let fnlen = rd_u16(b, off + 56) as usize;
            let fnoff = rd_u16(b, off + 58) as usize;
            let start = off + fnoff;
            if start + fnlen <= n {
                let name_u16: Vec<u16> = b[start..start + fnlen]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                nodes.insert(frn, (String::from_utf16_lossy(&name_u16), parent,
                                   attrs & FILE_ATTRIBUTE_DIRECTORY != 0));
            }
        }
        off += reclen;
    }
}

fn resolve(frn: u64, drive: &str, nodes: &FxHashMap<u64, (String, u64, bool)>,
           cache: &mut FxHashMap<u64, String>) -> String {
    if let Some(s) = cache.get(&frn) {
        return s.clone();
    }
    let mut chain: Vec<(u64, String)> = Vec::new();
    let mut cur = frn;
    let base;
    loop {
        if let Some(s) = cache.get(&cur) {
            base = s.clone();
            break;
        }
        match nodes.get(&cur) {
            None => {
                base = drive.to_string();
                break;
            }
            Some((name, parent, _)) => {
                if *parent == cur || !nodes.contains_key(parent) {
                    let p = format!("{}\\{}", drive, name);
                    cache.insert(cur, p.clone());
                    base = p;
                    break;
                }
                chain.push((cur, name.clone()));
                cur = *parent;
            }
        }
    }
    let mut acc = base;
    for (node_frn, name) in chain.iter().rev() {
        acc = format!("{}\\{}", acc, name);
        cache.insert(*node_frn, acc.clone());
    }
    cache.get(&frn).cloned().unwrap_or(acc)
}

fn query_next_usn(h: HANDLE) -> i64 {
    let mut out = [0u8; 80];
    let mut ret: DWORD = 0;
    let ok = unsafe {
        DeviceIoControl(h, FSCTL_QUERY_USN_JOURNAL, std::ptr::null(), 0,
                        out.as_mut_ptr() as *mut c_void, 80, &mut ret, std::ptr::null_mut())
    };
    if ok == 0 {
        return MAX_USN;
    }
    let next = rd_u64(&out, 16) as i64;
    if next > 0 { next } else { MAX_USN }
}

fn build_mft(drive: &str) -> Result<Vec<Raw>, DWORD> {
    let path = to_wide(&format!("\\\\.\\{}", drive));
    let h = unsafe {
        CreateFileW(path.as_ptr(), GENERIC_READ, FILE_SHARE_RW, std::ptr::null_mut(),
                    OPEN_EXISTING, 0, 0)
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(unsafe { GetLastError() });
    }
    let high = query_next_usn(h);
    let mut nodes: FxHashMap<u64, (String, u64, bool)> = FxHashMap::default();
    let bufsize = 1usize << 18;
    let mut buf = vec![0u8; bufsize];
    let mut start: u64 = 0;
    loop {
        let mut med = [0u8; 24];
        med[0..8].copy_from_slice(&start.to_le_bytes());
        med[16..24].copy_from_slice(&high.to_le_bytes());
        let mut ret: DWORD = 0;
        let ok = unsafe {
            DeviceIoControl(h, FSCTL_ENUM_USN_DATA, med.as_ptr() as *const c_void, 24,
                            buf.as_mut_ptr() as *mut c_void, bufsize as DWORD, &mut ret,
                            std::ptr::null_mut())
        };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            unsafe { CloseHandle(h) };
            if e == ERROR_HANDLE_EOF {
                break;
            }
            return Err(e);
        }
        let n = ret as usize;
        if n <= 8 {
            break;
        }
        start = rd_u64(&buf, 0);
        parse_usn(&buf[..n], &mut nodes);
    }
    unsafe { CloseHandle(h) };
    let mut cache: FxHashMap<u64, String> = FxHashMap::default();
    let mut out = Vec::with_capacity(nodes.len());
    for (&frn, (_n, _p, is_dir)) in &nodes {
        // USN carries no size/mtime -> 0 (stat'd lazily when a filter/output needs it).
        out.push((resolve(frn, drive, &nodes, &mut cache), *is_dir, 0u64, 0i64));
    }
    Ok(out)
}

// ------------------------------ Index type -------------------------------
struct Entry {
    full: String,
    full_lo: String, // lowercased full path: fast case-insensitive substring ops
    is_dir: bool,
    size: u64,
    mtime: i64, // unix secs; 0 = unknown (MFT), triggers a lazy stat when needed
}

fn leaf(path: &str) -> &str {
    match path.rfind('\\') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

fn parent(path: &str) -> &str {
    match path.rfind('\\') {
        Some(i) => &path[..i],
        None => "",
    }
}

fn ext_lower(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

fn stat_meta(path: &str) -> (u64, i64) {
    match std::fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (m.len(), mtime)
        }
        Err(_) => (0, 0),
    }
}

fn eff_meta(e: &Entry) -> (u64, i64) {
    if e.mtime != 0 {
        (e.size, e.mtime)
    } else {
        stat_meta(&e.full)
    }
}

fn finalize(v: Vec<Raw>) -> Vec<Entry> {
    v.into_par_iter()
        .map(|(full, is_dir, size, mtime)| {
            let full_lo = full.to_lowercase();
            Entry { full, full_lo, is_dir, size, mtime }
        })
        .collect()
}

// ------------------------------ Matching ---------------------------------
fn glob_to_regex(glob: &str) -> String {
    let mut s = String::with_capacity(glob.len() + 4);
    s.push('^');
    for c in glob.chars() {
        match c {
            '*' => s.push_str(".*"),
            '?' => s.push('.'),
            c => s.push_str(&regex::escape(&c.to_string())),
        }
    }
    s.push('$');
    s
}

enum Matcher {
    MatchAll,
    Substr(String),
    Re(Regex),
}

impl Matcher {
    fn new(query: &str, regex: bool, case: bool) -> Result<Matcher, String> {
        if query.is_empty() {
            return Ok(Matcher::MatchAll);
        }
        if regex {
            let re = RegexBuilder::new(query).case_insensitive(!case).build()
                .map_err(|e| format!("bad regex: {}", e))?;
            return Ok(Matcher::Re(re));
        }
        if query.contains('*') || query.contains('?') {
            let re = RegexBuilder::new(&glob_to_regex(query)).case_insensitive(!case).build()
                .map_err(|e| format!("bad glob: {}", e))?;
            return Ok(Matcher::Re(re));
        }
        Ok(Matcher::Substr(query.to_lowercase()))
    }

    fn hit(&self, e: &Entry, match_path: bool) -> bool {
        match self {
            Matcher::MatchAll => true,
            Matcher::Substr(q) => {
                let hay = if match_path { e.full_lo.as_str() } else { leaf(&e.full_lo) };
                hay.contains(q.as_str())
            }
            Matcher::Re(re) => {
                let hay = if match_path { e.full.as_str() } else { leaf(&e.full) };
                re.is_match(hay)
            }
        }
    }
}

// Built-in ignore substrings (matched against the lowercased full path).
const DEFAULT_IGNORES: &[&str] = &[
    "\\node_modules\\", "\\.git\\", "\\$recycle.bin\\", "\\cache\\", "\\caches\\",
    "\\.cache\\", ".facache", ".texcache", "\\tinytex\\", "\\steam\\",
    "\\appdata\\local\\temp\\",
];

// Whole-token containment: `term` must be bounded by non-alphanumeric chars (or
// string edges), so "toc" matches "\toc\" or "unit-toc.pdf" but NOT "CGMWTOCT".
fn contains_token(hay: &str, term: &str) -> bool {
    if term.is_empty() {
        return true;
    }
    let hb = hay.as_bytes();
    for (i, _) in hay.match_indices(term) {
        let before = i == 0 || !hb[i - 1].is_ascii_alphanumeric();
        let after_idx = i + term.len();
        let after = after_idx >= hb.len() || !hb[after_idx].is_ascii_alphanumeric();
        if before && after {
            return true;
        }
    }
    false
}

fn has_term(hay: &str, term: &str, whole_word: bool) -> bool {
    if whole_word {
        contains_token(hay, term)
    } else {
        hay.contains(term)
    }
}

struct Filter {
    matcher: Matcher,
    match_path: bool,
    whole_word: bool,
    exts: Vec<String>,
    all_of: Vec<String>,
    any_of: Vec<String>,
    none_of: Vec<String>,
    excludes: Vec<String>,
    hash_re: Option<Regex>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    modified_after: Option<i64>,
}

impl Filter {
    fn needs_meta(&self) -> bool {
        self.min_size.is_some() || self.max_size.is_some() || self.modified_after.is_some()
    }

    // Cheap, allocation-free predicate (no filesystem access).
    fn cheap_pass(&self, e: &Entry) -> bool {
        for ex in &self.excludes {
            if e.full_lo.contains(ex.as_str()) {
                return false;
            }
        }
        if let Some(re) = &self.hash_re {
            if re.is_match(leaf(&e.full_lo)) {
                return false;
            }
        }
        if !self.exts.is_empty() {
            if e.is_dir {
                return false;
            }
            let ex = ext_lower(leaf(&e.full_lo));
            if !self.exts.iter().any(|x| *x == ex) {
                return false;
            }
        }
        if !self.matcher.hit(e, self.match_path) {
            return false;
        }
        for t in &self.all_of {
            if !has_term(&e.full_lo, t, self.whole_word) {
                return false;
            }
        }
        if !self.any_of.is_empty()
            && !self.any_of.iter().any(|t| has_term(&e.full_lo, t, self.whole_word))
        {
            return false;
        }
        for t in &self.none_of {
            if has_term(&e.full_lo, t, self.whole_word) {
                return false;
            }
        }
        true
    }

    // Size/date predicate; may stat (only reached for cheap-pass candidates).
    fn meta_pass(&self, e: &Entry) -> bool {
        let (size, mtime) = eff_meta(e);
        if !e.is_dir {
            if let Some(mn) = self.min_size {
                if size < mn {
                    return false;
                }
            }
            if let Some(mx) = self.max_size {
                if size > mx {
                    return false;
                }
            }
        }
        if let Some(after) = self.modified_after {
            if mtime < after {
                return false;
            }
        }
        true
    }
}

struct QueryResult<'a> {
    total: usize,
    folders: usize,
    files: usize,
    hits: Vec<&'a Entry>,
    dirs: Vec<(String, usize)>,
}

fn run_query<'a>(index: &'a [Entry], f: &Filter, limit: usize, offset: usize) -> QueryResult<'a> {
    let cand: Vec<&Entry> = index.par_iter().filter(|e| f.cheap_pass(e)).collect();
    let matches: Vec<&Entry> = if f.needs_meta() {
        cand.into_par_iter().filter(|e| f.meta_pass(e)).collect()
    } else {
        cand
    };
    let total = matches.len();
    let folders = matches.iter().filter(|e| e.is_dir).count();
    let files = total - folders;

    let mut dir_counts: FxHashMap<&str, usize> = FxHashMap::default();
    for e in &matches {
        *dir_counts.entry(parent(&e.full)).or_insert(0) += 1;
    }
    let mut dirs: Vec<(String, usize)> = dir_counts.into_iter().map(|(d, c)| (d.to_string(), c)).collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    dirs.truncate(100);

    let take = if limit == 0 { usize::MAX } else { limit };
    let hits: Vec<&Entry> = matches.into_iter().skip(offset).take(take).collect();
    QueryResult { total, folders, files, hits, dirs }
}

// --------------------------- Filter construction -------------------------
fn str_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|e| e.as_str()).map(|s| s.to_lowercase()).collect())
        .unwrap_or_default()
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return Some(n); // already unix seconds
    }
    let parts: Vec<&str> = s.split(['-', '/', 'T', ' ', ':']).collect();
    if parts.len() >= 3 {
        let y: i64 = parts[0].parse().ok()?;
        let m: i64 = parts[1].parse().ok()?;
        let d: i64 = parts[2].parse().ok()?;
        return Some(days_from_civil(y, m, d) * 86400);
    }
    None
}

// Build a Filter + (limit, offset) from a JSON request object (shared by CLI,
// serve and MCP).
fn filter_from_value(v: &Value) -> Result<(Filter, usize, usize), String> {
    let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("");
    let regex = v.get("regex").and_then(|b| b.as_bool()).unwrap_or(false);
    let case = v.get("case").and_then(|b| b.as_bool()).unwrap_or(false);
    let match_path = v.get("match_path").and_then(|b| b.as_bool()).unwrap_or(false);
    let matcher = Matcher::new(query, regex, case)?;

    let exts: Vec<String> = str_array(v, "ext")
        .into_iter()
        .map(|e| e.trim_start_matches('.').to_string())
        .filter(|e| !e.is_empty())
        .collect();

    let mut excludes = str_array(v, "exclude");
    let no_default = v.get("no_default_ignores").and_then(|b| b.as_bool()).unwrap_or(false);
    let hash_re = if no_default {
        None
    } else {
        for ig in DEFAULT_IGNORES {
            excludes.push(ig.to_string());
        }
        // hashed cache filenames, e.g. Tectonic's 64-hex sha256 PDFs
        Regex::new(r"^[0-9a-f]{16,}(\.[0-9a-z0-9]+)?$").ok()
    };

    let limit = v.get("max_results").and_then(|n| n.as_u64()).unwrap_or(100) as usize;
    let offset = v.get("offset").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
    let modified_after = v
        .get("modified_after")
        .and_then(|x| x.as_str())
        .and_then(parse_date)
        .or_else(|| v.get("modified_after").and_then(|n| n.as_i64()));
    let mut all_of = str_array(v, "all_of");
    match v.get("root") {
        Some(Value::String(s)) => all_of.push(s.to_lowercase().replace('/', "\\")),
        Some(Value::Array(a)) => {
            for x in a {
                if let Some(s) = x.as_str() {
                    all_of.push(s.to_lowercase().replace('/', "\\"));
                }
            }
        }
        _ => {}
    }

    Ok((
        Filter {
            matcher,
            match_path,
            whole_word: v.get("whole_word").and_then(|b| b.as_bool()).unwrap_or(false),
            exts,
            all_of,
            any_of: str_array(v, "any_of"),
            none_of: str_array(v, "none_of"),
            excludes,
            hash_re,
            min_size: v.get("min_size").and_then(|n| n.as_u64()),
            max_size: v.get("max_size").and_then(|n| n.as_u64()),
            modified_after,
        },
        limit,
        offset,
    ))
}

// ------------------------------- Output ----------------------------------
fn result_json(backend: &str, r: &QueryResult, offset: usize) -> Value {
    let results: Vec<Value> = r
        .hits
        .iter()
        .map(|e| {
            let (size, mtime) = eff_meta(e);
            let name = leaf(&e.full);
            json!({
                "path": e.full,
                "name": name,
                "dir": parent(&e.full),
                "ext": ext_lower(name),
                "size": size,
                "mtime": mtime,
                "is_folder": e.is_dir,
            })
        })
        .collect();
    let mut dirs = Map::new();
    for (d, c) in &r.dirs {
        dirs.insert(d.clone(), json!(c));
    }
    json!({
        "backend": backend,
        "total": r.total,
        "folders": r.folders,
        "files": r.files,
        "offset": offset,
        "count": r.hits.len(),
        "results": results,
        "dirs": Value::Object(dirs),
    })
}

// ------------------------------- Content grep ----------------------------
struct GrepHit {
    path: String,
    line_no: usize,
    line: String,
}

struct FileHit {
    path: String,
    line_no: usize,
    line: String,
}

struct GrepOpts {
    max_file_size: u64,
    max_scan_bytes: usize,
    per_file_cap: usize,
    limit: usize,
    max_files: usize,
    io_threads: usize,
    budget: std::time::Duration,
    files_only: bool,
    group_by_dir: bool,
    skip_deps: bool,
}

enum GrepData {
    Lines(Vec<GrepHit>),
    Files(Vec<FileHit>),
}

struct GrepResult {
    pattern: String,
    candidates: usize,
    files_scanned: usize,
    files_matched: usize,
    total: usize,
    truncated: bool,
    scanned_capped: bool,
    timed_out: bool,
    complete: bool,
    group_by_dir: bool,
    deps_skipped: bool,
    data: GrepData,
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

// Package-manager / tooling trees that content grep skips by default (skip_deps).
// They hold third-party or generated code, not the user's own; scanning them is
// what turns "which files import X" into a minutes-long whole-disk read.
const DEP_SEGMENTS: [&str; 10] = [
    "\\site-packages\\", "\\dist-packages\\", "\\node_modules\\", "\\.venv\\",
    "\\venv\\", "\\__pycache__\\", "\\.tox\\", "\\.git\\", ".egg-info\\", ".dist-info\\",
];
fn is_dep_path(full_lo: &str) -> bool {
    if DEP_SEGMENTS.iter().any(|s| full_lo.contains(s)) {
        return true;
    }
    // CPython standard library: ...\pythonNN\lib\... - never first-party, but guard
    // against an unrelated user folder literally named "lib".
    full_lo.contains("\\lib\\") && (full_lo.contains("\\python3") || full_lo.contains("\\python2"))
}

// Read at most `cap` bytes (0 = whole file). Most content matches - imports in
// particular - live near the top, so a head cap bounds the cost of the rare
// multi-MB generated file without changing results in practice.
fn read_capped(path: &str, cap: usize) -> Option<Vec<u8>> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    if cap == 0 {
        let mut f = f;
        f.read_to_end(&mut buf).ok()?;
    } else {
        f.take(cap as u64).read_to_end(&mut buf).ok()?;
    }
    Some(buf)
}

// Compile a content matcher + read/scan options from a JSON request.
fn grep_regex(v: &Value) -> Result<(Regex, GrepOpts), String> {
    let pattern = v.get("pattern").and_then(|p| p.as_str()).unwrap_or("");
    if pattern.is_empty() {
        return Err("pattern is required".into());
    }
    let is_regex = v.get("regex").and_then(|b| b.as_bool()).unwrap_or(false);
    let ignore_case = v.get("ignore_case").and_then(|b| b.as_bool()).unwrap_or(true);
    let whole_word = v.get("whole_word").and_then(|b| b.as_bool()).unwrap_or(false);
    let mut pat = if is_regex { pattern.to_string() } else { regex::escape(pattern) };
    if whole_word {
        pat = format!(r"\b(?:{})\b", pat);
    }
    let re = RegexBuilder::new(&pat)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| format!("bad pattern: {}", e))?;
    let group_by_dir = v.get("group_by_dir").and_then(|b| b.as_bool()).unwrap_or(false);
    let files_only = group_by_dir || v.get("files_only").and_then(|b| b.as_bool()).unwrap_or(false);
    let max_file_size = v.get("max_file_size").and_then(|n| n.as_u64()).unwrap_or(0);
    let max_scan_bytes = v.get("max_scan_bytes").and_then(|n| n.as_u64()).unwrap_or(1_048_576) as usize;
    let per_file_cap = v.get("per_file_cap").and_then(|n| n.as_u64()).unwrap_or(50) as usize;
    let max_files = v.get("max_files").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
    let io_threads = (v.get("io_threads").and_then(|n| n.as_u64()).unwrap_or(32) as usize).clamp(1, 256);
    let budget_secs = v.get("time_budget_secs").and_then(|n| n.as_u64()).unwrap_or(22);
    let req_limit = v.get("max_results").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
    let limit = if req_limit > 0 { req_limit } else if files_only { 5000 } else { 500 };
    let skip_deps = !v.get("include_deps").and_then(|b| b.as_bool()).unwrap_or(false)
        && v.get("skip_deps").and_then(|b| b.as_bool()).unwrap_or(true);
    Ok((re, GrepOpts {
        max_file_size, max_scan_bytes, per_file_cap, limit, max_files, io_threads,
        budget: std::time::Duration::from_secs(budget_secs), files_only, group_by_dir, skip_deps,
    }))
}

// Grep file *contents*. Candidate files come from the in-RAM filename index
// (scoped by ext / root / path filters), so only relevant files are opened, and
// the reads run on a wide I/O pool because this phase is disk-bound, not CPU-bound.
fn run_grep(index: &[Entry], f: &Filter, re: &Regex, o: &GrepOpts) -> GrepResult {
    let mut cands: Vec<&Entry> = index.par_iter()
        .filter(|e| !e.is_dir && f.cheap_pass(e) && !(o.skip_deps && is_dep_path(&e.full_lo)))
        .collect();
    let candidates = cands.len();
    let scanned_capped = o.max_files > 0 && candidates > o.max_files;
    if scanned_capped {
        cands.truncate(o.max_files);
    }
    let deadline = Instant::now() + o.budget;
    let timed = std::sync::atomic::AtomicBool::new(false);

    let scan = |e: &&Entry| -> (bool, Vec<GrepHit>) {
        if Instant::now() >= deadline {
            timed.store(true, std::sync::atomic::Ordering::Relaxed);
            return (false, Vec::new());
        }
        if o.max_file_size > 0 {
            let (size, _) = eff_meta(e);
            if size > o.max_file_size {
                return (false, Vec::new());
            }
        }
        let bytes = match read_capped(&e.full, o.max_scan_bytes) {
            Some(b) => b,
            None => return (false, Vec::new()),
        };
        if is_binary(&bytes) {
            return (true, Vec::new());
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut hits = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let mut l = line.trim().to_string();
                if l.chars().count() > 200 {
                    l = l.chars().take(200).collect::<String>() + "…";
                }
                hits.push(GrepHit { path: e.full.clone(), line_no: i + 1, line: l });
                if o.files_only || hits.len() >= o.per_file_cap {
                    break;
                }
            }
        }
        (true, hits)
    };

    let per: Vec<(bool, Vec<GrepHit>)> = match rayon::ThreadPoolBuilder::new().num_threads(o.io_threads).build() {
        Ok(p) => p.install(|| cands.par_iter().map(&scan).collect()),
        Err(_) => cands.par_iter().map(&scan).collect(),
    };

    let mut files_scanned = 0usize;
    let mut files_matched = 0usize;
    let mut line_hits: Vec<GrepHit> = Vec::new();
    let mut file_hits: Vec<FileHit> = Vec::new();
    for (scanned, hits) in per {
        if scanned {
            files_scanned += 1;
        }
        if !hits.is_empty() {
            files_matched += 1;
            if o.files_only {
                let h = &hits[0];
                file_hits.push(FileHit { path: h.path.clone(), line_no: h.line_no, line: h.line.clone() });
            } else {
                line_hits.extend(hits);
            }
        }
    }

    let timed_out = timed.load(std::sync::atomic::Ordering::Relaxed);
    let complete = !timed_out && !scanned_capped;
    let (total, truncated, data) = if o.files_only {
        let truncated = o.limit > 0 && file_hits.len() > o.limit;
        if truncated {
            file_hits.truncate(o.limit);
        }
        (files_matched, truncated, GrepData::Files(file_hits))
    } else {
        let total = line_hits.len();
        let truncated = o.limit > 0 && total > o.limit;
        if truncated {
            line_hits.truncate(o.limit);
        }
        (total, truncated, GrepData::Lines(line_hits))
    };

    GrepResult {
        pattern: re.as_str().to_string(),
        candidates, files_scanned, files_matched, total, truncated,
        scanned_capped, timed_out, complete, group_by_dir: o.group_by_dir, deps_skipped: o.skip_deps, data,
    }
}

fn grep_json(r: &GrepResult) -> Value {
    let mut out = Map::new();
    out.insert("pattern".into(), json!(r.pattern));
    out.insert("candidates".into(), json!(r.candidates));
    out.insert("files_scanned".into(), json!(r.files_scanned));
    out.insert("files_matched".into(), json!(r.files_matched));
    out.insert("total".into(), json!(r.total));
    out.insert("truncated".into(), json!(r.truncated));
    out.insert("scanned_capped".into(), json!(r.scanned_capped));
    out.insert("timed_out".into(), json!(r.timed_out));
    out.insert("complete".into(), json!(r.complete));
    out.insert("deps_skipped".into(), json!(r.deps_skipped));
    out.insert("note".into(), json!(match (r.complete, r.deps_skipped) {
        (false, _) => "partial: raise time_budget_secs / io_threads or scope with root/ext; scanned files are now OS-cached, so a re-run finishes fast",
        (true, true) => "complete over first-party code; dependency/library trees (site-packages, node_modules, .venv, ...) were skipped - pass include_deps:true to scan them too",
        (true, false) => "",
    }));
    match &r.data {
        GrepData::Files(files) => {
            if r.group_by_dir {
                use std::collections::BTreeMap;
                let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                for fh in files {
                    groups.entry(parent(&fh.path)).or_default().push(leaf(&fh.path));
                }
                let by_dir: Vec<Value> = groups.into_iter()
                    .map(|(dir, names)| json!({"dir": dir, "count": names.len(), "files": names}))
                    .collect();
                out.insert("dirs".into(), json!(by_dir.len()));
                out.insert("count".into(), json!(files.len()));
                out.insert("by_dir".into(), json!(by_dir));
            } else {
                let results: Vec<Value> = files.iter().map(|fh| json!({
                    "path": fh.path, "dir": parent(&fh.path), "name": leaf(&fh.path),
                    "line_no": fh.line_no, "line": fh.line,
                })).collect();
                out.insert("count".into(), json!(results.len()));
                out.insert("results".into(), json!(results));
            }
        }
        GrepData::Lines(hits) => {
            let results: Vec<Value> = hits.iter().map(|h| json!({
                "path": h.path, "dir": parent(&h.path), "name": leaf(&h.path),
                "line_no": h.line_no, "line": h.line,
            })).collect();
            out.insert("count".into(), json!(results.len()));
            out.insert("results".into(), json!(results));
        }
    }
    Value::Object(out)
}

// ------------------------------ Index build ------------------------------
fn cache_path(key: &str) -> String {
    let dir = env::var("TEMP").unwrap_or_else(|_| ".".into());
    let mut hash: u64 = 1469598103934665603;
    for b in key.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{}\\omp_fastfind2_{:016x}.idx", dir, hash)
}

fn save_cache(path: &str, index: &[Raw]) {
    let mut buf = String::with_capacity(index.len() * 48);
    for (full, is_dir, size, mtime) in index {
        buf.push(if *is_dir { 'D' } else { 'F' });
        buf.push_str(&size.to_string());
        buf.push('\t');
        buf.push_str(&mtime.to_string());
        buf.push('\t');
        buf.push_str(full);
        buf.push('\n');
    }
    let _ = std::fs::write(path, buf);
}

fn load_cache(path: &str) -> Option<Vec<Raw>> {
    let mut s = String::new();
    std::fs::File::open(path).ok()?.read_to_string(&mut s).ok()?;
    let mut out = Vec::new();
    for line in s.lines() {
        if line.len() < 2 {
            continue;
        }
        let is_dir = line.as_bytes()[0] == b'D';
        let rest = &line[1..];
        let mut it = rest.splitn(3, '\t');
        let size: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let mtime: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let full = match it.next() {
            Some(p) => p.to_string(),
            None => continue,
        };
        out.push((full, is_dir, size, mtime));
    }
    Some(out)
}

fn get_index(backend: &str, roots: &[String], threads: usize, refresh: bool,
             log: bool) -> Result<(Vec<Raw>, String), String> {
    let (chosen, key) = if backend == "mft" {
        ("mft".to_string(), "mft".to_string())
    } else {
        let key = format!(
            "walk|{}",
            roots.iter().map(|r| normalize_root(r).to_lowercase()).collect::<Vec<_>>().join("|")
        );
        ("walk".to_string(), key)
    };
    let cpath = cache_path(&key);
    if !refresh {
        if let Some(idx) = load_cache(&cpath) {
            if log {
                eprintln!("[{}] loaded {} entries from cache", chosen, idx.len());
            }
            return Ok((idx, chosen));
        }
    }
    let t = Instant::now();
    let idx = if chosen == "mft" {
        let drives = ntfs_drives();
        if drives.is_empty() {
            return Err("no NTFS volumes for mft backend".into());
        }
        let mut all = Vec::new();
        for d in &drives {
            match build_mft(d) {
                Ok(mut v) => all.append(&mut v),
                Err(ERROR_ACCESS_DENIED) => {
                    return Err(format!(
                        "access denied opening {} - mft backend requires Administrator", d))
                }
                Err(e) => return Err(format!("mft enumeration of {} failed (error {})", d, e)),
            }
        }
        all
    } else {
        build_walk(roots, threads)
    };
    if log {
        eprintln!("[{}] built {} entries in {:.2}s", chosen, idx.len(), t.elapsed().as_secs_f64());
    }
    save_cache(&cpath, &idx);
    Ok((idx, chosen))
}

// -------------------------------- Serve ----------------------------------
// Each stdin line is a JSON request object (same fields as the MCP tool);
// one JSON result line is returned per request.
fn serve(index: &[Entry], backend: &str) {
    let stdout = io::stdout();
    let mut w = stdout.lock();
    let _ = writeln!(w, "READY\t{}\t{}", backend, index.len());
    let _ = w.flush();
    for line in io::stdin().lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(w, "{}", json!({"error": format!("bad json: {}", e)}));
                let _ = w.flush();
                continue;
            }
        };
        let is_grep = req.get("pattern").and_then(|p| p.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
        let out = match filter_from_value(&req) {
            Ok((filter, limit, offset)) => {
                if is_grep {
                    match grep_regex(&req) {
                        Ok((re, opts)) => grep_json(&run_grep(index, &filter, &re, &opts)),
                        Err(e) => json!({"error": e}),
                    }
                } else {
                    result_json(backend, &run_query(index, &filter, limit, offset), offset)
                }
            }
            Err(e) => json!({"error": e}),
        };
        let _ = writeln!(w, "{}", out);
        let _ = w.flush();
    }
}

// -------------------------------- Main -----------------------------------
fn can_use_mft() -> bool {
    match ntfs_drives().first() {
        None => false,
        Some(d) => {
            let path = to_wide(&format!("\\\\.\\{}", d));
            let h = unsafe {
                CreateFileW(path.as_ptr(), GENERIC_READ, FILE_SHARE_RW, std::ptr::null_mut(),
                            OPEN_EXISTING, 0, 0)
            };
            if h == INVALID_HANDLE_VALUE {
                false
            } else {
                unsafe { CloseHandle(h) };
                true
            }
        }
    }
}

fn default_threads() -> usize {
    let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    (cores * 4).clamp(4, 64)
}

fn build_default_index() -> Result<(Vec<Entry>, String), String> {
    let backend = if can_use_mft() { "mft" } else { "walk" };
    let roots = fixed_roots();
    let (raw, chosen) = get_index(backend, &roots, default_threads(), false, false)?;
    Ok((finalize(raw), chosen))
}

// ------------------------------- MCP server ------------------------------
fn mcp_send(v: &Value) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(v.to_string().as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn mcp_result(id: &Value, result: Value) {
    mcp_send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn mcp_error(id: &Value, code: i64, message: &str) {
    mcp_send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
}

fn mcp_tool_schema() -> Value {
    json!({
        "name": "search",
        "description": "Ultra-fast file/folder search across the whole machine (NTFS $MFT when elevated, else a parallel directory walk; index built once, kept in RAM). Declare intent with structured filters instead of path regex. Built-in ignores (node_modules, .git, caches, TinyTeX, Steam, hashed cache filenames) are on by default. Returns structured JSON: per-hit {path,name,dir,ext,size,mtime,is_folder} plus a dirs:{path:count} rollup for grouping.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Optional name match. Substring by default; * ? wildcards; or a regex when regex=true. Omit to match everything (then filter with the fields below)."},
                "ext": {"type": "array", "items": {"type": "string"}, "description": "File extensions to include, e.g. [\"pdf\",\"docx\"] (no dot, no regex)."},
                "all_of": {"type": "array", "items": {"type": "string"}, "description": "AND terms: every term must appear in the full path (case-insensitive)."},
                "any_of": {"type": "array", "items": {"type": "string"}, "description": "OR terms: at least one must appear in the full path."},
                "none_of": {"type": "array", "items": {"type": "string"}, "description": "NOT terms: drop hits whose full path contains any of these."},
                "min_size": {"type": "integer", "description": "Minimum file size in bytes (drops tiny cache/font files)."},
                "max_size": {"type": "integer", "description": "Maximum file size in bytes."},
                "modified_after": {"type": "string", "description": "Only files modified on/after this date (YYYY-MM-DD or unix seconds)."},
                "exclude": {"type": "array", "items": {"type": "string"}, "description": "Extra path substrings to ignore, on top of the built-in ignore rules."},
                "no_default_ignores": {"type": "boolean", "description": "Disable built-in ignore rules (search caches/node_modules/etc too)."},
                "regex": {"type": "boolean", "description": "Treat query as a regular expression."},
                "match_path": {"type": "boolean", "description": "Match query against the full path instead of just the file name."},
                "root": {"type": "string", "description": "Scope results to files/folders under this directory."},
                "whole_word": {"type": "boolean", "description": "Match any_of/all_of/none_of terms only as whole tokens bounded by non-alphanumeric characters (so \"toc\" matches \\toc\\ but not CGMWTOCT)."},
                "max_results": {"type": "integer", "description": "Max results returned (default 100; 0 = all). Note: total/dirs always reflect the full match count."}
            }
        }
    })
}

fn grep_tool_schema() -> Value {
    json!({
        "name": "grep",
        "description": "Search file CONTENTS across the whole machine. The in-RAM filename index selects only in-scope files (root/ext/path filters); their contents are read on a wide I/O pool. Best for 'where is X used' and 'which files contain/import X'. For a grouped-by-folder answer set group_by_dir:true and get one compact map in a SINGLE call - do NOT split into many scoped calls. Set files_only:true when you just need the file list.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Text to find inside files. Literal by default; set regex:true for a regular expression."},
                "root": {"type": "string", "description": "Scope to this directory. Only files under it are read."},
                "ext": {"type": "array", "items": {"type": "string"}, "description": "Restrict to these file extensions, e.g. [\"py\",\"ts\"]."},
                "all_of": {"type": "array", "items": {"type": "string"}, "description": "Only scan files whose full path contains all of these."},
                "any_of": {"type": "array", "items": {"type": "string"}, "description": "Only scan files whose full path contains at least one of these."},
                "none_of": {"type": "array", "items": {"type": "string"}, "description": "Skip files whose full path contains any of these."},
                "regex": {"type": "boolean", "description": "Treat pattern as a regular expression."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default true)."},
                "whole_word": {"type": "boolean", "description": "Match the pattern only as a whole word."},
                "files_only": {"type": "boolean", "description": "Return one entry per matching file (path + first hit) instead of every line. Much smaller output."},
                "group_by_dir": {"type": "boolean", "description": "Return matches grouped by folder as by_dir:[{dir,count,files}]. Implies files_only. Use this for 'grouped by folder' in a single call."},
                "skip_deps": {"type": "boolean", "description": "Skip dependency/library trees - site-packages, node_modules, .venv, __pycache__, .git, *.egg-info/.dist-info, and the CPython stdlib (default true). Keeps a whole-machine scan fast and first-party."},
                "include_deps": {"type": "boolean", "description": "Also scan the dependency/library trees that skip_deps hides. Slower and mostly third-party code."},
                "max_scan_bytes": {"type": "integer", "description": "Read at most this many bytes per file (default 1048576; 0 = whole file). Matches past the cap are missed."},
                "io_threads": {"type": "integer", "description": "Parallel file readers (default 32). Higher helps on SSDs since scanning is I/O-bound."},
                "time_budget_secs": {"type": "integer", "description": "Wall-clock ceiling for one call (default 22, safe under a 30s MCP timeout). Raise it (and the server's mcp.json timeout) to finish a whole-machine scan in one call; result.complete says whether coverage was full."},
                "max_files": {"type": "integer", "description": "Cap candidate files scanned (default 0 = unlimited)."},
                "per_file_cap": {"type": "integer", "description": "Max matching lines kept per file in line mode (default 50)."},
                "max_results": {"type": "integer", "description": "Cap returned rows (default 500 lines, or 5000 files in files_only/group_by_dir)."}
            },
            "required": ["pattern"]
        }
    })
}

fn mcp_serve() {
    eprintln!("[fastfind] MCP server up; building index in the background");
    // Build the whole-machine index off-thread so no tools/call blocks past the
    // client's transport timeout. Calls return a fast "still building" notice
    // until the index is ready.
    let (tx, rx) = std::sync::mpsc::channel::<Result<(Vec<Entry>, String), String>>();
    std::thread::spawn(move || {
        let _ = tx.send(build_default_index());
    });
    let started = Instant::now();
    let mut index: Option<(Vec<Entry>, String)> = None;
    let mut build_err: Option<String> = None;

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if index.is_none() && build_err.is_none() {
            match rx.try_recv() {
                Ok(Ok(v)) => index = Some(v),
                Ok(Err(e)) => build_err = Some(e),
                Err(_) => {}
            }
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                mcp_error(&Value::Null, -32700, "parse error");
                continue;
            }
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = match msg.get("id").cloned() {
            Some(v) if !v.is_null() => v,
            _ => continue,
        };
        match method {
            "initialize" => mcp_result(&id, json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fastfind", "version": "0.5.0"}
            })),
            "ping" => mcp_result(&id, json!({})),
            "tools/list" => mcp_result(&id, json!({"tools": [mcp_tool_schema(), grep_tool_schema()]})),
            "tools/call" => {
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                if name != "search" && name != "grep" {
                    mcp_error(&id, -32601, "unknown tool");
                    continue;
                }
                if let Some(e) = &build_err {
                    mcp_result(&id, json!({"content": [{"type": "text", "text": format!("error building index: {}", e)}], "isError": true}));
                    continue;
                }
                if index.is_none() {
                    let secs = started.elapsed().as_secs();
                    mcp_result(&id, json!({"content": [{"type": "text", "text": format!(
                        "fastfind is still building its in-RAM index ({}s elapsed). The whole-machine build runs once; retry this same call in a few seconds and it will return instantly.", secs)}],
                        "isError": false}));
                    continue;
                }
                let (filter, limit, offset) = match filter_from_value(&args) {
                    Ok(v) => v,
                    Err(e) => {
                        mcp_result(&id, json!({"content": [{"type": "text", "text": format!("error: {}", e)}], "isError": true}));
                        continue;
                    }
                };
                let (idx, label) = index.as_ref().unwrap();
                let payload = if name == "grep" {
                    match grep_regex(&args) {
                        Ok((re, opts)) => grep_json(&run_grep(idx, &filter, &re, &opts)),
                        Err(e) => {
                            mcp_result(&id, json!({"content": [{"type": "text", "text": format!("error: {}", e)}], "isError": true}));
                            continue;
                        }
                    }
                } else {
                    let limit = if limit == 0 || limit > 500 { 500 } else { limit };
                    result_json(label, &run_query(idx, &filter, limit, offset), offset)
                };
                mcp_result(&id, json!({
                    "content": [{"type": "text", "text": serde_json::to_string(&payload).unwrap()}],
                    "isError": false
                }));
            }
            _ => mcp_error(&id, -32601, "method not found"),
        }
    }
}

// ------------------------------- Installer -------------------------------
fn install(apply: bool, only: Option<&str>) {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "fastfind.exe".into());
    let home = env::var("USERPROFILE").unwrap_or_default();
    let appdata = env::var("APPDATA").unwrap_or_default();

    let targets: Vec<(&str, String, &str, Value, String)> = vec![
        ("omp", format!("{}\\.omp\\agent\\mcp.json", home), "mcpServers",
         json!({"type": "stdio", "command": exe.clone(), "args": ["--mcp"]}),
         format!("{}\\.omp", home)),
        ("claude-desktop", format!("{}\\Claude\\claude_desktop_config.json", appdata), "mcpServers",
         json!({"command": exe.clone(), "args": ["--mcp"]}),
         format!("{}\\Claude", appdata)),
        ("claude-code", format!("{}\\.claude.json", home), "mcpServers",
         json!({"command": exe.clone(), "args": ["--mcp"]}),
         format!("{}\\.claude.json", home)),
        ("opencode", format!("{}\\.config\\opencode\\opencode.json", home), "mcp",
         json!({"type": "local", "command": [exe.clone(), "--mcp"], "enabled": true}),
         format!("{}\\.config\\opencode", home)),
        ("cursor", format!("{}\\.cursor\\mcp.json", home), "mcpServers",
         json!({"command": exe.clone(), "args": ["--mcp"]}),
         format!("{}\\.cursor", home)),
    ];

    for (name, path, container, entry, marker) in &targets {
        if let Some(o) = only {
            if o != *name {
                continue;
            }
        }
        let file_exists = Path::new(path).exists();
        let present = only.is_some() || file_exists || Path::new(marker).exists();
        if !apply {
            println!("{:<15} {}  [config: {}, agent present: {}]", name, path, file_exists, present);
            println!("    under \"{}\": \"fastfind\": {}", container, entry);
            continue;
        }
        if !present {
            println!("{:<15} skipped (agent not detected; use --agent {} to force)", name, name);
            continue;
        }
        let mut root = if file_exists {
            let text = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => { println!("{:<15} SKIP: cannot read {} ({})", name, path, e); continue; }
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(v) if v.is_object() => v,
                Ok(_) => { println!("{:<15} SKIP: {} is not a JSON object; not overwriting", name, path); continue; }
                Err(e) => { println!("{:<15} SKIP: {} not valid JSON ({}); not overwriting", name, path, e); continue; }
            }
        } else {
            json!({})
        };
        if file_exists {
            let _ = std::fs::copy(path, format!("{}.bak", path));
        }
        {
            let obj = root.as_object_mut().unwrap();
            let cont = obj.entry(container.to_string()).or_insert_with(|| json!({}));
            if !cont.is_object() {
                *cont = json!({});
            }
            cont.as_object_mut().unwrap().insert("fastfind".to_string(), entry.clone());
        }
        if let Some(dir) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(path, serde_json::to_string_pretty(&root).unwrap()) {
            Ok(_) => println!("{:<15} registered -> {}{}", name, path,
                              if file_exists { "  (backup: .bak)" } else { "  (created)" }),
            Err(e) => println!("{:<15} FAILED: {}", name, e),
        }
    }
    if !apply {
        println!("\n(dry run) add --apply to write, or --agent <name> to target one agent.");
    }
}

fn arg_val(argv: &[String], i: &mut usize) -> Option<String> {
    *i += 1;
    argv.get(*i).cloned()
}

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();

    if argv.first().map(|s| s.as_str()) == Some("install") {
        let apply = argv.iter().any(|a| a == "--apply");
        let only = argv.iter().position(|a| a == "--agent").and_then(|i| argv.get(i + 1)).cloned();
        install(apply, only.as_deref());
        return;
    }
    if argv.iter().any(|a| a == "--mcp") {
        mcp_serve();
        return;
    }

    let mut query: Option<String> = None;
    let mut roots: Vec<String> = Vec::new();
    let mut backend = "auto".to_string();
    let mut threads = 0usize;
    let mut refresh = false;
    let mut serve_mode = false;
    let mut json_out = false;
    let mut count = false;
    // request fields
    let mut req = Map::new();
    let mut exts: Vec<Value> = Vec::new();
    let mut any_of: Vec<Value> = Vec::new();
    let mut all_of: Vec<Value> = Vec::new();
    let mut none_of: Vec<Value> = Vec::new();
    let mut exclude: Vec<Value> = Vec::new();
    let mut root_terms: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--root" => { if let Some(v) = arg_val(&argv, &mut i) { roots.push(v.clone()); root_terms.push(json!(v)); } }
            "--threads" => { if let Some(v) = arg_val(&argv, &mut i) { threads = v.parse().unwrap_or(0); } }
            "-n" | "--limit" => { if let Some(v) = arg_val(&argv, &mut i) { req.insert("max_results".into(), json!(v.parse::<u64>().unwrap_or(100))); } }
            "--offset" => { if let Some(v) = arg_val(&argv, &mut i) { req.insert("offset".into(), json!(v.parse::<u64>().unwrap_or(0))); } }
            "--ext" => { if let Some(v) = arg_val(&argv, &mut i) { for e in v.split(',') { exts.push(json!(e)); } } }
            "--any" => { if let Some(v) = arg_val(&argv, &mut i) { any_of.push(json!(v)); } }
            "--all" => { if let Some(v) = arg_val(&argv, &mut i) { all_of.push(json!(v)); } }
            "--none" => { if let Some(v) = arg_val(&argv, &mut i) { none_of.push(json!(v)); } }
            "--exclude" => { if let Some(v) = arg_val(&argv, &mut i) { exclude.push(json!(v)); } }
            "--min-size" => { if let Some(v) = arg_val(&argv, &mut i) { req.insert("min_size".into(), json!(v.parse::<u64>().unwrap_or(0))); } }
            "--max-size" => { if let Some(v) = arg_val(&argv, &mut i) { req.insert("max_size".into(), json!(v.parse::<u64>().unwrap_or(u64::MAX))); } }
            "--after" => { if let Some(v) = arg_val(&argv, &mut i) { req.insert("modified_after".into(), json!(v)); } }
            "-g" | "--grep" => { if let Some(v) = arg_val(&argv, &mut i) { req.insert("pattern".into(), json!(v)); } }
            "--max-file-size" => { if let Some(v) = arg_val(&argv, &mut i) { req.insert("max_file_size".into(), json!(v.parse::<u64>().unwrap_or(2_000_000))); } }
            "--no-ignore" => { req.insert("no_default_ignores".into(), json!(true)); }
            "-p" | "--path" => { req.insert("match_path".into(), json!(true)); }
            "-W" | "--whole-word" => { req.insert("whole_word".into(), json!(true)); }
            "-r" | "--regex" => { req.insert("regex".into(), json!(true)); }
            "-i" | "--case" => { req.insert("case".into(), json!(true)); }
            "--mft" => backend = "mft".into(),
            "--walk" => backend = "walk".into(),
            "--auto" => backend = "auto".into(),
            "--json" => json_out = true,
            "--count" => count = true,
            "--refresh" => refresh = true,
            "--serve" => serve_mode = true,
            other => { if query.is_none() { query = Some(other.to_string()); } }
        }
        i += 1;
    }
    if !exts.is_empty() { req.insert("ext".into(), Value::Array(exts)); }
    if !any_of.is_empty() { req.insert("any_of".into(), Value::Array(any_of)); }
    if !all_of.is_empty() { req.insert("all_of".into(), Value::Array(all_of)); }
    if !none_of.is_empty() { req.insert("none_of".into(), Value::Array(none_of)); }
    if !exclude.is_empty() { req.insert("exclude".into(), Value::Array(exclude)); }
    if !root_terms.is_empty() { req.insert("root".into(), Value::Array(root_terms)); }
    if let Some(q) = &query { req.insert("query".into(), json!(q)); }

    if threads == 0 {
        threads = default_threads();
    }
    if backend == "auto" {
        backend = if can_use_mft() { "mft".into() } else { "walk".into() };
    }
    if backend == "walk" && roots.is_empty() {
        roots = fixed_roots();
    }

    let (raw, chosen) = match get_index(&backend, &roots, threads, refresh, true) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(2);
        }
    };
    let index = finalize(raw);

    if serve_mode {
        serve(&index, &chosen);
        return;
    }

    let req = Value::Object(req);
    let (filter, limit, offset) = match filter_from_value(&req) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(2);
        }
    };
    // content grep mode when a --grep pattern was given
    if req.get("pattern").and_then(|p| p.as_str()).map(|s| !s.is_empty()).unwrap_or(false) {
        let (re, opts) = match grep_regex(&req) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(2);
            }
        };
        let g = run_grep(&index, &filter, &re, &opts);
        if count {
            println!("{}", g.total);
        } else if json_out {
            println!("{}", serde_json::to_string_pretty(&grep_json(&g)).unwrap());
        } else {
            let stdout = io::stdout();
            let mut w = io::BufWriter::new(stdout.lock());
            match &g.data {
                GrepData::Lines(hits) => {
                    for h in hits {
                        let _ = writeln!(w, "{}:{}: {}", h.path, h.line_no, h.line);
                    }
                }
                GrepData::Files(files) => {
                    for fh in files {
                        let _ = writeln!(w, "{}", fh.path);
                    }
                }
            }
            let _ = w.flush();
            eprintln!("[grep] {} matches in {} files ({} scanned){}",
                      g.total, g.files_matched, g.files_scanned,
                      if g.truncated { " - truncated" } else { "" });
        }
        return;
    }

    let r = run_query(&index, &filter, limit, offset);

    if count {
        println!("{}", r.total);
    } else if json_out {
        println!("{}", serde_json::to_string_pretty(&result_json(&chosen, &r, offset)).unwrap());
    } else {
        let stdout = io::stdout();
        let mut w = io::BufWriter::new(stdout.lock());
        for e in &r.hits {
            let _ = writeln!(w, "{}", e.full);
        }
        let _ = w.flush();
        eprintln!("[{}] {} shown / {} total ({} folders, {} files)",
                  chosen, r.hits.len(), r.total, r.folders, r.files);
    }
}
