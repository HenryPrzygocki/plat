// SPDX-License-Identifier: GPL-3.0-or-later
//! File extension interning and classification.
//!
//! Every file node stores a `u16` extension id instead of re-parsing its name.
//! Ids come from one process-wide table, so they stay valid across rescans.
//! Scanner threads keep a thread-local cache in front of the table, which means
//! the shared lock is only taken the first time a thread meets an extension.

use std::cell::RefCell;
use std::sync::{LazyLock, Mutex};

use rustc_hash::FxHashMap;

/// Broad file category. Drives tile colour in the treemap.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Cat {
    Other = 0,
    Video,
    Audio,
    Image,
    Archive,
    Document,
    Code,
    Data,
    Binary,
    Temp,
}

impl Cat {
    pub const COUNT: usize = 10;
    pub const ALL: [Cat; Cat::COUNT] = [
        Cat::Other,
        Cat::Video,
        Cat::Audio,
        Cat::Image,
        Cat::Archive,
        Cat::Document,
        Cat::Code,
        Cat::Data,
        Cat::Binary,
        Cat::Temp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Cat::Other => "other",
            Cat::Video => "video",
            Cat::Audio => "audio",
            Cat::Image => "image",
            Cat::Archive => "archive",
            Cat::Document => "document",
            Cat::Code => "code",
            Cat::Data => "data",
            Cat::Binary => "binary",
            Cat::Temp => "temp / log",
        }
    }
}

/// No extension.
pub const EXT_NONE: u16 = 0;
/// Pseudo extension for extensionless files with an executable bit.
pub const EXT_EXEC: u16 = 1;
/// Longest extension we bother to track, in bytes.
const MAX_EXT_LEN: usize = 10;
/// Past this many distinct extensions, new ones are folded into "none". Random
/// suffixes (temp files, hashes) would otherwise grow the table without bound.
const MAX_EXTS: usize = 16_384;

struct Table {
    names: Vec<Box<str>>,
    cats: Vec<Cat>,
    map: FxHashMap<u128, u16>,
}

static TABLE: LazyLock<Mutex<Table>> = LazyLock::new(|| {
    Mutex::new(Table {
        names: vec!["".into(), "executable".into()],
        cats: vec![Cat::Other, Cat::Binary],
        map: FxHashMap::default(),
    })
});

thread_local! {
    static CACHE: RefCell<FxHashMap<u128, u16>> = RefCell::new(FxHashMap::default());
}

/// Extract the lowercased extension of `name`, packed into a `u128` key.
///
/// Trailing numbered components are skipped, so `libfoo.so.1.2` yields `so`,
/// `dump.tar.zst.part03` yields `zst` and `dump.0001` yields nothing. Hidden files like `.bashrc` have no
/// extension.
fn ext_key(name: &[u8]) -> Option<u128> {
    let mut end = name.len();
    loop {
        let dot = name[..end].iter().rposition(|&b| b == b'.')?;
        if dot == 0 {
            return None;
        }
        let ext = &name[dot + 1..end];
        if ext.is_empty() || ext.len() > MAX_EXT_LEN {
            return None;
        }
        // Numbered pieces: `x.7z.001`, `lib.so.6`, `dump.tar.zst.part03`.
        let digits = ext.strip_prefix(b"part").unwrap_or(ext);
        if digits.iter().all(u8::is_ascii_digit) {
            end = dot;
            continue;
        }
        if !ext
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'+')
        {
            return None;
        }
        let mut buf = [0u8; 16];
        for (dst, src) in buf.iter_mut().zip(ext) {
            *dst = src.to_ascii_lowercase();
        }
        return Some(u128::from_le_bytes(buf));
    }
}

/// Intern the extension of a file name. `executable` marks a file with any
/// execute permission bit, used only when the name has no extension.
pub fn intern(name: &[u8], executable: bool) -> u16 {
    let Some(key) = ext_key(name) else {
        return if executable { EXT_EXEC } else { EXT_NONE };
    };
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(&id) = cache.get(&key) {
            return id;
        }
        let id = intern_slow(key);
        cache.insert(key, id);
        id
    })
}

fn intern_slow(key: u128) -> u16 {
    let mut table = TABLE.lock().unwrap();
    if let Some(&id) = table.map.get(&key) {
        return id;
    }
    if table.names.len() >= MAX_EXTS {
        return EXT_NONE;
    }
    let bytes = key.to_le_bytes();
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    // The key only ever holds ASCII, so this cannot fail.
    let name = std::str::from_utf8(&bytes[..len]).unwrap_or("");
    let id = table.names.len() as u16;
    table.cats.push(classify(name));
    table.names.push(name.into());
    table.map.insert(key, id);
    id
}

/// Snapshot of the category of every extension id known so far.
pub fn categories() -> Vec<Cat> {
    TABLE.lock().unwrap().cats.clone()
}

/// Display name of an extension id.
pub fn name(id: u16) -> Box<str> {
    let table = TABLE.lock().unwrap();
    table.names.get(id as usize).cloned().unwrap_or_default()
}

fn classify(ext: &str) -> Cat {
    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg"
        | "m2ts" | "mts" | "vob" | "ogv" | "3gp" | "rmvb" | "divx" => Cat::Video,

        "mp3" | "flac" | "wav" | "ogg" | "opus" | "m4a" | "aac" | "wma" | "aiff" | "aif"
        | "ape" | "alac" | "mid" | "midi" | "wv" | "dsf" => Cat::Audio,

        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "tif" | "tiff" | "webp" | "svg" | "heic"
        | "heif" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "psd"
        | "xcf" | "ico" | "avif" | "jxl" | "eps" | "ai" | "kra" | "exr" | "hdr" | "tga" => {
            Cat::Image
        }

        "zip" | "tar" | "gz" | "tgz" | "bz2" | "tbz2" | "xz" | "txz" | "zst" | "zstd" | "7z"
        | "rar" | "lz" | "lz4" | "lzma" | "lzo" | "cab" | "deb" | "rpm" | "pkg" | "iso" | "img"
        | "dmg" | "qcow2" | "vdi" | "vmdk" | "vhd" | "vhdx" | "squashfs" | "jar" | "war"
        | "whl" | "apk" | "cpio" | "snap" | "flatpak" | "crate" | "gem" | "egg" | "nupkg"
        | "xpi" | "ova" => Cat::Archive,

        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "tex" | "ppt" | "pptx" | "odp"
        | "xls" | "xlsx" | "ods" | "epub" | "mobi" | "azw3" | "djvu" | "ps" | "org" | "rst"
        | "bib" | "pages" | "numbers" | "chm" | "html" | "htm" | "xhtml" | "man" => Cat::Document,

        "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "hh" | "rs" | "py" | "pyx" | "pyi" | "js"
        | "mjs" | "cjs" | "ts" | "jsx" | "tsx" | "go" | "java" | "kt" | "swift" | "rb" | "php"
        | "lua" | "pl" | "pm" | "sh" | "bash" | "zsh" | "fish" | "vim" | "el" | "lisp" | "clj"
        | "hs" | "ml" | "scala" | "cs" | "fs" | "f" | "f90" | "f95" | "f03" | "f08" | "for"
        | "jl" | "r" | "m" | "ipynb" | "json" | "yaml" | "yml" | "toml" | "xml" | "ini" | "cfg"
        | "conf" | "css" | "scss" | "sass" | "less" | "sql" | "cmake" | "mk" | "gradle" | "nix"
        | "proto" | "tf" | "vue" | "svelte" | "dart" | "zig" | "asm" | "s" | "pro" | "idl"
        | "ex" | "exs" | "erl" | "nim" | "cu" | "cuh" | "glsl" | "wgsl" | "patch" | "diff" => {
            Cat::Code
        }

        "csv" | "tsv" | "dat" | "h5" | "hdf5" | "hdf" | "he5" | "nc" | "nc4" | "cdf" | "fits"
        | "fit" | "fts" | "npy" | "npz" | "parquet" | "feather" | "arrow" | "pkl" | "pickle"
        | "mat" | "sav" | "root" | "db" | "sqlite" | "sqlite3" | "mdb" | "sdf" | "vtk" | "vtu"
        | "vti" | "vtp" | "vtr" | "vts" | "pvd" | "pvtu" | "xdmf" | "xmf" | "bp" | "grib"
        | "grb" | "grib2" | "zarr" | "lmdb" | "dbf" | "shp" | "geojson" | "tfrecord" | "pt"
        | "pth" | "ckpt" | "safetensors" | "onnx" | "gguf" | "rds" | "dta" | "jsonl" | "ndjson"
        | "xdr" | "orc" | "avro" | "ldb" | "sst" | "pack" | "idx" | "joblib" | "silo" | "cgns"
        | "exo" | "tec" | "plt" | "dmp" => Cat::Data,

        "so" | "a" | "o" | "ko" | "exe" | "dll" | "dylib" | "lib" | "obj" | "bin" | "elf"
        | "appimage" | "pyc" | "pyo" | "pyd" | "class" | "wasm" | "node" | "rlib" | "rmeta"
        | "efi" | "sys" | "msi" | "out" | "debug" | "dwo" | "pdb" | "gch" | "pch" | "mod"
        | "typelib" | "beam" => Cat::Binary,

        "log" | "tmp" | "temp" | "bak" | "old" | "swp" | "swo" | "cache" | "lock" | "pid"
        | "journal" | "core" | "crash" | "part" | "crdownload" | "download" | "orig" | "rej"
        | "trace" | "err" | "stackdump" => Cat::Temp,

        _ => Cat::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(name: &str) -> String {
        ext_key(name.as_bytes())
            .map(|k| {
                let b = k.to_le_bytes();
                let n = b.iter().position(|&c| c == 0).unwrap();
                String::from_utf8_lossy(&b[..n]).into_owned()
            })
            .unwrap_or_default()
    }

    #[test]
    fn extracts_extensions() {
        assert_eq!(ext("movie.MKV"), "mkv");
        assert_eq!(ext("archive.tar.gz"), "gz");
        assert_eq!(ext("libfoo.so.1.2.3"), "so");
        assert_eq!(ext("dump.0001"), "");
        assert_eq!(ext("data.tar.zst.part03"), "zst");
        assert_eq!(ext("half.part"), "");
        assert_eq!(ext(".bashrc"), "");
        assert_eq!(ext("noext"), "");
        assert_eq!(ext("weird.name with space"), "");
        assert_eq!(ext("trailing."), "");
        assert_eq!(ext("a.waytoolongextension"), "");
    }

    #[test]
    fn interning_is_stable_and_classified() {
        let a = intern(b"one.hdf5", false);
        let b = intern(b"two.HDF5", false);
        assert_eq!(a, b);
        assert_eq!(&*name(a), "hdf5");
        assert_eq!(categories()[a as usize], Cat::Data);
        assert_eq!(intern(b"tool", true), EXT_EXEC);
        assert_eq!(intern(b"README", false), EXT_NONE);
    }
}
