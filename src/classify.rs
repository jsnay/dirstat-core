//! ============================================================================
//! FILE: src/classify.rs
//!
//! ============================================================================
//!
//! # Purpose
//! Kind classification: the 8 stable, learnable categories that replace
//! WinDirStat's "top 12 extensions this scan" coloring (design 1g). The
//! engine owns *which category a node belongs to* (a fact about the data,
//! derived from a curated extension + path-rule table); the host owns the
//! RGB each category gets. The key design decision is the two-tier rule
//! system: directory-name path rules set an inherited category for whole
//! subtrees ("where it lives"), and only files with no inherited category
//! fall back to their own extension ("what it is") — which is what makes a
//! `.mov` inside `Caches` count as System junk rather than Media. Rules are
//! portable and deterministic; hosts with richer type systems (macOS UTIs)
//! can refine later.
//!
//! # Upstream dependencies (what this file consumes)
//! - (none — pure functions over strings; no std facilities beyond core
//!   string ops, no other crate modules)
//!
//! # Downstream consumers (who depends on this file)
//! - src/scan.rs — calls `category_for_dir_name` on every directory as it
//!   descends (threading the result down as the inherited category) and
//!   `classify_file` on every file at insert time; stores `Category as u8`
//!   in `Node.category`
//! - src/ffi.rs — `Category::from_u8`/`name` behind `ds_category_name`;
//!   `CATEGORY_COUNT` sizes the `ds_category_list` aggregation
//! - src/lib.rs — re-exports `Category`
//! - tests/engine.rs — asserts path-rule propagation end-to-end
//!
//! # Structure
//! - `Category` / `CATEGORY_COUNT` — the stable 8-bucket id space (ABI)
//! - `Category::from_u8` / `Category::name` — decode + stable English label
//! - `category_for_dir_name` — path-segment rules for directories/bundles
//! - `category_for_extension` — the curated extension table for files
//! - `classify_file` — the precedence rule: inherited (path) beats extension
//! - tests — precedence and bundle-classification unit checks
//!
//! # Algorithm & invariants
//! - Precedence: path rule (nearest ancestor with a match, deeper wins)
//!   over file extension over `Other`. Enforced by `classify_file` taking
//!   `inherited` first, and by scan.rs overriding the inherited value with
//!   `dir_cat.or(inherited)` as it descends.
//! - Category ids are ABI-frozen (they cross FFI in `DsNodeInfo.category`
//!   and index the host palette): never renumber, only append.
//! - All matching is deterministic and case-handled explicitly: exact
//!   (case-sensitive) names for real-world artifacts like `DerivedData`,
//!   lowercased comparisons everywhere else.
//!
//! ============================================================================

/// Stable category ids. The numeric values are ABI: they cross the FFI
/// boundary in `DsNodeInfo.category` and index the host's palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Category {
    /// Build products, dependency caches, VM/container disks, source trees.
    Developer = 0,
    /// Audio & video, DAW/NLE project bundles.
    Media = 1,
    /// Photo libraries and camera formats.
    Photos = 2,
    /// Documents: text, office, design.
    Documents = 3,
    /// Archives & disk images: things you can usually delete after install.
    Archives = 4,
    /// System data, caches, logs, backups.
    System = 5,
    /// Applications and bundles.
    Apps = 6,
    /// Everything else.
    Other = 7,
}

/// Number of categories; sizes fixed-length aggregation arrays (e.g. the
/// `ds_category_list` output, which always returns exactly this many rows).
pub const CATEGORY_COUNT: usize = 8;

impl Category {
    /// Decode a raw category byte (e.g. from FFI or `Node.category`).
    /// Unknown values collapse to `Other` rather than erroring, keeping
    /// old hosts forward-compatible with engines that add categories.
    pub fn from_u8(v: u8) -> Category {
        match v {
            0 => Category::Developer,
            1 => Category::Media,
            2 => Category::Photos,
            3 => Category::Documents,
            4 => Category::Archives,
            5 => Category::System,
            6 => Category::Apps,
            _ => Category::Other,
        }
    }

    /// Stable English name (hosts localize their own labels).
    pub fn name(self) -> &'static str {
        match self {
            Category::Developer => "Developer",
            Category::Media => "Audio & Video",
            Category::Photos => "Photos",
            Category::Documents => "Documents",
            Category::Archives => "Archives & Images",
            Category::System => "System & Caches",
            Category::Apps => "Applications",
            Category::Other => "Other",
        }
    }
}

/// Path-segment rules, checked on every *directory* name as the scan
/// descends. A match sets the "inherited" category for everything below
/// (unless a deeper rule overrides). This is how DerivedData is "Developer"
/// and Caches is "System" regardless of the extensions inside.
///
/// Returns `None` for directories with no rule, which means "keep whatever
/// category was inherited from above" (see the `.or` chaining in scan.rs).
pub fn category_for_dir_name(name: &str) -> Option<Category> {
    // Case-sensitive where the real-world artifact is (macOS conventions),
    // otherwise lowercase comparisons.
    match name {
        "DerivedData"
        | "node_modules"
        | ".git"
        | "target"
        | ".build"
        | "Pods"
        | "ModuleCache.noindex"
        | "Index.noindex"
        | ".gradle"
        | ".cargo"
        | ".rustup"
        | ".npm"
        | ".pnpm-store"
        | "venv"
        | ".venv"
        | "__pycache__" => return Some(Category::Developer),
        "Caches" | "Logs" | "CrashReporter" | "MobileSync" | "Backups.backupdb"
        | "CloudStorage" | "Trash" | ".Trash" => return Some(Category::System),
        "Applications" => return Some(Category::Apps),
        _ => {}
    }
    let lower = name.to_ascii_lowercase();
    // Bundle-style directory extensions classify the whole bundle
    // ("classified as one thing" — design 1g): a .photoslibrary is Photos
    // all the way down even though its guts are sqlite and plists.
    if let Some(ext) = lower.rsplit_once('.').map(|(_, e)| e) {
        return match ext {
            "app" | "appex" | "xpc" | "framework" | "dylib" => Some(Category::Apps),
            "photoslibrary" | "photolibrary" => Some(Category::Photos),
            "logicx" | "fcpbundle" | "imovielibrary" | "band" | "tvlibrary" => {
                Some(Category::Media)
            }
            "xcodeproj" | "xcworkspace" | "playground" | "swiftpm" => Some(Category::Developer),
            "pvm" | "vmwarevm" | "utm" | "docker" => Some(Category::Developer),
            "sparsebundle" | "backupbundle" => Some(Category::System),
            _ => None,
        };
    }
    None
}

/// Extension → category for plain files. Lowercase input expected (the
/// scanner lowercases in `extension_of`, giving the case-insensitive merge
/// the spec requires). Unknown extensions map to `Other`.
pub fn category_for_extension(ext: &str) -> Category {
    match ext {
        // Developer
        "o" | "a" | "obj" | "lib" | "swiftmodule" | "swiftdoc" | "pcm" | "noindex"
        | "xcactivitylog" | "class" | "jar" | "wasm" | "so" | "rlib" | "rmeta" | "crate" | "c"
        | "h" | "cpp" | "hpp" | "m" | "mm" | "swift" | "rs" | "go" | "py" | "js" | "ts" | "tsx"
        | "jsx" | "map" | "node" | "raw" | "qcow2" | "vmdk" | "vdi" | "hdd" | "img" => {
            Category::Developer
        }
        // Audio & video
        "mov" | "mp4" | "m4v" | "avi" | "mkv" | "webm" | "braw" | "r3d" | "mxf" | "prores"
        | "mp3" | "m4a" | "aac" | "wav" | "aif" | "aiff" | "flac" | "ogg" | "logicx" | "als"
        | "flp" | "ptx" | "sesx" | "caf" => Category::Media,
        // Photos
        "jpg" | "jpeg" | "png" | "heic" | "heif" | "tiff" | "tif" | "gif" | "bmp" | "webp"
        | "cr2" | "cr3" | "nef" | "arw" | "raf" | "orf" | "dng" | "rw2" | "psd" | "svg" => {
            Category::Photos
        }
        // Documents
        "pdf" | "txt" | "md" | "rtf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "key"
        | "pages" | "numbers" | "sketch" | "fig" | "epub" | "csv" | "json" | "xml" | "yaml"
        | "yml" | "toml" | "html" | "css" => Category::Documents,
        // Archives & disk images
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "xip" | "dmg"
        | "iso" | "pkg" | "mpkg" | "deb" | "rpm" | "appimage" => Category::Archives,
        // System & caches
        "log" | "cache" | "db" | "db-wal" | "db-shm" | "sqlite" | "sqlite-wal" | "sqlite-shm"
        | "plist" | "backup" | "asl" | "diag" | "ips" | "crash" | "tmp" | "swap" | "dat" => {
            Category::System
        }
        // Applications (loose executables)
        "app" | "exe" | "appx" => Category::Apps,
        _ => Category::Other,
    }
}

/// Classify a file given its lowercase extension (if any) and the category
/// inherited from path rules above it. Path rules win: a `.js` file inside
/// `node_modules` is Developer either way, but a `.mov` inside `Caches`
/// stays System — "where it lives" beats "what it is" for junk detection.
pub fn classify_file(ext: Option<&str>, inherited: Option<Category>) -> Category {
    if let Some(c) = inherited {
        return c;
    }
    match ext {
        Some(e) => category_for_extension(e),
        None => Category::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The precedence contract of `classify_file`: an inherited path-rule
    /// category always wins; extension only applies when nothing is
    /// inherited.
    #[test]
    fn path_rules_beat_extensions() {
        assert_eq!(
            classify_file(Some("js"), Some(Category::Developer)),
            Category::Developer
        );
        assert_eq!(
            classify_file(Some("mov"), Some(Category::System)),
            Category::System
        );
        assert_eq!(classify_file(Some("mov"), None), Category::Media);
    }

    /// Bundle-style directory names classify the whole subtree (design 1g);
    /// plain names with no rule ("Documents") classify nothing.
    #[test]
    fn bundles_classify_as_one_thing() {
        assert_eq!(
            category_for_dir_name("Photos Library.photoslibrary"),
            Some(Category::Photos)
        );
        assert_eq!(category_for_dir_name("Xcode.app"), Some(Category::Apps));
        assert_eq!(
            category_for_dir_name("Album_Master.logicx"),
            Some(Category::Media)
        );
        assert_eq!(
            category_for_dir_name("DerivedData"),
            Some(Category::Developer)
        );
        assert_eq!(category_for_dir_name("Documents"), None);
    }
}
