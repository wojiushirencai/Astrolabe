//! Swift / Objective-C language-server setup that a bare checkout lacks.
//!
//! * **clangd for ObjC** needs compile flags. Without a
//!   `compile_commands.json` it parses `.m` as plain C (no `-fobjc-arc`, no
//!   header search paths, `.h` in C mode) and cross-file references come back
//!   nearly empty. When the repository has no compilation database we write a
//!   synthetic one under `.astrolabe/clangd/` and point clangd at it with
//!   `--compile-commands-dir`. A repo's own database always wins.
//! * **Off macOS** there is no Apple SDK, so `#import <Foundation/Foundation.h>`
//!   is a fatal error that stops clang before the first declaration. We
//!   generate stub framework headers (one shared prelude declaring
//!   `NSObject`, `BOOL`, `NS_ENUM`, …) so the translation unit keeps parsing
//!   and the project's own symbols get indexed.
//! * **sourcekit-lsp** only fills its index from a build; background indexing
//!   (enabled through `initializationOptions`, see `transport.rs`) runs the
//!   SwiftPM "prepare" step itself.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::scan::{scan, ScanOptions};
use crate::types::{Language, RelPath};

use super::ServerSpec;

/// Directory (under the repo root) holding the synthetic clangd setup.
pub const CLANGD_DIR: &str = ".astrolabe/clangd";
const PRELUDE: &str = "__astrolabe_objc_prelude.h";
/// Bound the synthetic database: header search paths are one `-I` per dir.
const MAX_FILES: usize = 50_000;
const MAX_INCLUDE_DIRS: usize = 2_000;

/// Adjust a discovered server spec for Swift / ObjC before it is spawned.
pub fn prepare_spec(spec: &ServerSpec, root: &Path) -> ServerSpec {
    let mut spec = spec.clone();
    if !matches!(spec.language, Language::ObjC | Language::ObjCpp) || !is_clangd(&spec.command) {
        return spec;
    }
    if !spec
        .args
        .iter()
        .any(|a| a.starts_with("--background-index"))
    {
        spec.args.push("--background-index".into());
    }
    if spec
        .args
        .iter()
        .any(|a| a.starts_with("--compile-commands-dir"))
        || has_own_compile_db(root)
    {
        return spec;
    }
    match write_objc_compile_db(root, apple_sdk().as_deref()) {
        Ok(dir) => spec
            .args
            .push(format!("--compile-commands-dir={}", dir.display())),
        Err(err) => {
            tracing::warn!(error = %err, "could not write synthetic ObjC compile_commands.json")
        }
    }
    spec
}

/// A source file to open right after clangd starts for ObjC.
///
/// clangd loads the compilation database, and so starts its background
/// index, only when the first document is opened. Until then it reports no
/// progress, the pool decides it is idle, and the first references query is
/// answered from an index that has not started yet. Opening one `.m` from the
/// synthetic database at spawn makes indexing (and its `$/progress`) begin
/// immediately, so the readiness wait sees it.
pub fn warm_up_file(spec: &ServerSpec, root: &Path) -> Option<RelPath> {
    if !matches!(spec.language, Language::ObjC | Language::ObjCpp) || !is_clangd(&spec.command) {
        return None;
    }
    let db = std::fs::read_to_string(root.join(CLANGD_DIR).join("compile_commands.json")).ok()?;
    let entries: Vec<Value> = serde_json::from_str(&db).ok()?;
    entries.iter().find_map(|e| {
        let file = Path::new(e.get("file")?.as_str()?);
        if !file.extension().is_some_and(|x| x == "m" || x == "mm") {
            return None;
        }
        let rel = file.strip_prefix(root).ok()?;
        Some(RelPath::new(rel.to_string_lossy().replace('\\', "/")))
    })
}

fn is_clangd(command: &Path) -> bool {
    command
        .file_stem()
        .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("clangd"))
}

/// The repository ships its own flags (CMake export, `bear`, xcpretty, …).
pub fn has_own_compile_db(root: &Path) -> bool {
    [
        "compile_commands.json",
        "build/compile_commands.json",
        "compile_flags.txt",
    ]
    .iter()
    .any(|p| root.join(p).is_file())
}

/// `xcrun --show-sdk-path` on macOS; `None` elsewhere or without Xcode.
fn apple_sdk() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::process::Command::new("xcrun")
        .arg("--show-sdk-path")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !path.is_empty()).then_some(path)
}

/// Write `<root>/.astrolabe/clangd/compile_commands.json` (and, without an
/// SDK, stub framework headers). Returns the directory to hand to clangd.
pub fn write_objc_compile_db(root: &Path, sdk: Option<&str>) -> std::io::Result<PathBuf> {
    let dir = root.join(CLANGD_DIR);
    std::fs::create_dir_all(&dir)?;
    let mut files: Vec<RelPath> = scan(root, &ScanOptions::default())
        .files
        .into_iter()
        .filter(|f| {
            f.extension()
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "m" | "mm" | "h"))
        })
        .collect();
    files.sort();
    files.truncate(MAX_FILES);

    let mut include_dirs = BTreeSet::new();
    let mut angle_imports = BTreeSet::new();
    let mut entries = Vec::new();
    for file in &files {
        let abs = root.join(file.as_str());
        if let Some(parent) = abs.parent() {
            if include_dirs.len() < MAX_INCLUDE_DIRS {
                include_dirs.insert(parent.to_path_buf());
            }
        }
        let source = std::fs::read_to_string(&abs).unwrap_or_default();
        if sdk.is_none() {
            collect_angle_imports(&source, &mut angle_imports);
        }
        let ext = file.extension().unwrap_or_default().to_ascii_lowercase();
        let lang = match ext.as_str() {
            "mm" => "objective-c++",
            "m" => "objective-c",
            // Only ObjC headers (same sniff the indexer uses) are compiled as
            // ObjC; C/C++ headers in a mixed repo keep clangd's own inference.
            _ => match crate::parse::objc_header_language(&source) {
                Some(Language::ObjCpp) => "objective-c++",
                Some(_) => "objective-c",
                None => continue,
            },
        };
        entries.push((abs, lang));
    }

    let mut base: Vec<String> = vec![
        "-fobjc-arc".into(),
        "-ferror-limit=0".into(),
        "-Wno-everything".into(),
    ];
    match sdk {
        Some(sdk) => {
            base.push("-isysroot".into());
            base.push(sdk.to_string());
            base.push("-fmodules".into());
        }
        None => {
            let shim = dir.join("shim");
            write_shim(root, &shim, &angle_imports)?;
            // The GNU runtime rejects ARC; use the Apple runtime model.
            base.push("-fobjc-runtime=macosx".into());
            base.push(format!("-I{}", shim.display()));
            base.push("-include".into());
            base.push(shim.join(PRELUDE).display().to_string());
            for def in [
                "TARGET_OS_IOS=1",
                "TARGET_OS_IPHONE=1",
                "TARGET_OS_MAC=1",
                "TARGET_OS_OSX=0",
                "TARGET_OS_TV=0",
                "TARGET_OS_WATCH=0",
                "TARGET_OS_VISION=0",
                "TARGET_OS_MACCATALYST=0",
                "TARGET_OS_SIMULATOR=0",
            ] {
                base.push(format!("-D{def}"));
            }
        }
    }
    base.extend(include_dirs.iter().map(|d| format!("-I{}", d.display())));

    let commands: Vec<Value> = entries
        .iter()
        .map(|(abs, lang)| {
            let mut args = vec!["clang".to_string(), "-x".into(), (*lang).into()];
            args.extend(base.iter().cloned());
            args.push("-c".into());
            args.push(abs.display().to_string());
            json!({
                "directory": root.display().to_string(),
                "file": abs.display().to_string(),
                "arguments": args,
            })
        })
        .collect();
    let body = serde_json::to_string(&commands).map_err(std::io::Error::other)?;
    write_if_changed(&dir.join("compile_commands.json"), &body)?;
    Ok(dir)
}

/// `<Framework/Header.h>` imports that the repository does not provide.
fn collect_angle_imports(source: &str, out: &mut BTreeSet<String>) {
    for line in source.lines() {
        let line = line.trim_start();
        let rest = ["#import", "#include", "# import", "# include"]
            .iter()
            .find_map(|p| line.strip_prefix(p));
        let Some(rest) = rest else { continue };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('<') else {
            continue;
        };
        let Some(end) = rest.find('>') else { continue };
        let header = &rest[..end];
        if header.contains('/') && !header.contains("..") && !header.starts_with('/') {
            out.insert(header.to_string());
        }
    }
}

fn write_shim(root: &Path, shim: &Path, imports: &BTreeSet<String>) -> std::io::Result<()> {
    std::fs::create_dir_all(shim)?;
    write_if_changed(&shim.join(PRELUDE), OBJC_PRELUDE)?;
    for header in imports {
        // Framework-style imports of the project's own headers
        // (`<AFNetworking/AFURLSessionManager.h>`) resolve through the
        // project's `-I` dirs; only stub what the repo cannot provide.
        let file_name = header.rsplit('/').next().unwrap_or(header);
        if find_named(root, file_name) {
            continue;
        }
        let path = shim.join(header);
        if path.is_file() {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, format!("#include \"{PRELUDE}\"\n"))?;
    }
    Ok(())
}

fn find_named(root: &Path, file_name: &str) -> bool {
    // Cheap: only the immediate layout most ObjC repos use.
    walk_has(root, file_name, 4)
}

fn walk_has(dir: &Path, file_name: &str, depth: u32) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_file() && name == file_name {
            return true;
        }
        if ft.is_dir() && depth > 0 && walk_has(&entry.path(), file_name, depth - 1) {
            return true;
        }
    }
    false
}

fn write_if_changed(path: &Path, body: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|old| old == body) {
        return Ok(());
    }
    std::fs::write(path, body)
}

/// Just enough of Foundation for clang to keep parsing project code. Types
/// it does not know become ordinary (non-fatal) errors; the project's own
/// classes, protocols and methods still get indexed.
const OBJC_PRELUDE: &str = r#"#pragma once
// Generated by Astrolabe: minimal Apple SDK stand-in for indexing off macOS.
typedef signed char BOOL;
typedef long NSInteger;
typedef unsigned long NSUInteger;
typedef double CGFloat;
typedef double NSTimeInterval;
typedef struct CGPoint { CGFloat x, y; } CGPoint;
typedef struct CGSize { CGFloat width, height; } CGSize;
typedef struct CGRect { CGPoint origin; CGSize size; } CGRect;
typedef struct _NSRange { NSUInteger location, length; } NSRange;
@class NSString, NSArray, NSMutableArray, NSDictionary, NSMutableDictionary, NSSet,
    NSError, NSURL, NSData, NSNumber, NSDate, NSCoder, NSBundle, NSOperationQueue,
    NSURLRequest, NSURLResponse, NSURLSession, NSURLSessionTask, NSProgress, NSIndexSet,
    UIImage, UIView, UIViewController, UIColor, NSImage, NSView;
@protocol NSObject
@end
@protocol NSCopying
@end
@protocol NSMutableCopying
@end
@protocol NSCoding
@end
@protocol NSSecureCoding <NSCoding>
@end
@protocol NSFastEnumeration
@end
__attribute__((objc_root_class))
@interface NSObject <NSObject>
+ (instancetype)alloc;
+ (instancetype)new;
- (instancetype)init;
@end
#define NS_ASSUME_NONNULL_BEGIN _Pragma("clang assume_nonnull begin")
#define NS_ASSUME_NONNULL_END _Pragma("clang assume_nonnull end")
#define NS_DESIGNATED_INITIALIZER
#define NS_UNAVAILABLE
#define NS_REQUIRES_SUPER
#define NS_SWIFT_NAME(...)
#define NS_SWIFT_UNAVAILABLE(...)
#define NS_REFINED_FOR_SWIFT
#define NS_EXTENSIBLE_STRING_ENUM
#define NS_TYPED_ENUM
#define NS_TYPED_EXTENSIBLE_ENUM
#define NS_ENUM(_type, _name) enum _name : _type _name; enum _name : _type
#define NS_OPTIONS(_type, _name) enum _name : _type _name; enum _name : _type
#define NS_CLOSED_ENUM(_type, _name) enum _name : _type _name; enum _name : _type
#define NS_ERROR_ENUM(_dom, _name) enum _name : NSInteger
#define FOUNDATION_EXPORT extern
#define FOUNDATION_EXTERN extern
#define UIKIT_EXTERN extern
#define NS_INLINE static inline
#define API_AVAILABLE(...)
#define API_UNAVAILABLE(...)
#define API_DEPRECATED(...)
#define API_DEPRECATED_WITH_REPLACEMENT(...)
#define NS_AVAILABLE(...)
#define NS_AVAILABLE_IOS(...)
#define NS_DEPRECATED(...)
#define NS_DEPRECATED_IOS(...)
#define __IOS_AVAILABLE(...)
#define __TVOS_PROHIBITED
#define __WATCHOS_PROHIBITED
#define YES ((BOOL)1)
#define NO ((BOOL)0)
#ifndef nil
#define nil ((id)0)
#endif
#ifndef NULL
#define NULL ((void *)0)
#endif
"#;

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "astrolabe-apple-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }

    fn objc_spec(cmd: &str) -> ServerSpec {
        ServerSpec {
            language: Language::ObjC,
            command: PathBuf::from(cmd),
            args: Vec::new(),
            install_hint: String::new(),
        }
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn clangd_for_objc_gets_synthetic_compile_db() {
        let t = tmp("cdb");
        write(
            &t.0,
            "Lib/A.h",
            "#import <Foundation/Foundation.h>\n#import <Lib/B.h>\n@interface A : NSObject\n@end\n",
        );
        write(
            &t.0,
            "Lib/B.h",
            "#import <Foundation/Foundation.h>\n@interface B : NSObject\n@end\n",
        );
        write(
            &t.0,
            "Lib/A.m",
            "#import \"A.h\"\n@implementation A\n@end\n",
        );
        write(&t.0, "Lib/c.h", "int plain(void);\n");
        let spec = prepare_spec(&objc_spec("/usr/bin/clangd"), &t.0);
        assert!(spec.args.contains(&"--background-index".to_string()));
        let dir = spec
            .args
            .iter()
            .find_map(|a| a.strip_prefix("--compile-commands-dir="))
            .expect("compile-commands-dir");
        let db: Vec<Value> = serde_json::from_str(
            &std::fs::read_to_string(Path::new(dir).join("compile_commands.json")).unwrap(),
        )
        .unwrap();
        let files: Vec<String> = db
            .iter()
            .map(|e| e["file"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(
            !files.iter().any(|f| f.ends_with("c.h")),
            "plain C header stays out"
        );
        let args: Vec<String> = serde_json::from_value(db[0]["arguments"].clone()).unwrap();
        assert!(args.contains(&"objective-c".to_string()));
        assert!(args.contains(&"-fobjc-arc".to_string()));
        assert!(args
            .iter()
            .any(|a| a.starts_with("-I") && a.ends_with("Lib")));
        assert_eq!(
            warm_up_file(&spec, &t.0).map(|r| r.as_str().to_string()),
            Some("Lib/A.m".to_string())
        );
        if apple_sdk().is_none() {
            let shim = Path::new(dir).join("shim");
            assert!(shim.join("Foundation/Foundation.h").is_file());
            // The project's own framework-style header is not stubbed.
            assert!(!shim.join("Lib/B.h").exists());
            assert!(args.contains(&"-fobjc-runtime=macosx".to_string()));
        }
    }

    #[test]
    fn own_compile_db_and_other_servers_are_left_alone() {
        let t = tmp("own");
        write(&t.0, "A.m", "@implementation A\n@end\n");
        write(&t.0, "compile_commands.json", "[]");
        let spec = prepare_spec(&objc_spec("clangd"), &t.0);
        assert_eq!(spec.args, vec!["--background-index".to_string()]);
        assert!(!t.0.join(CLANGD_DIR).exists());

        let sk = prepare_spec(&objc_spec("sourcekit-lsp"), &t.0);
        assert!(sk.args.is_empty());
        let mut c = objc_spec("clangd");
        c.language = Language::C;
        assert!(prepare_spec(&c, &t.0).args.is_empty());
    }

    #[test]
    fn angle_imports_skip_quoted_and_system_c_headers() {
        let mut out = BTreeSet::new();
        collect_angle_imports(
            "#import <UIKit/UIKit.h>\n#include <stdio.h>\n#import \"Local.h\"\n  #import <A/B.h> // x\n",
            &mut out,
        );
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec!["A/B.h".to_string(), "UIKit/UIKit.h".to_string()]
        );
    }
}
