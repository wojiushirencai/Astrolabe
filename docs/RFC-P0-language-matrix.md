# RFC: P0 Language Matrix

- **Status:** Locked
- **Scope:** P0 language-server discovery, installation, and lifecycle behavior
- **Decision:** This RFC records the decisions that P0 implementation and review must follow. It does not expand the P0 language set.

## 1. Locked P0 language set

P0 covers the following language families:

| Family | P0 coverage | Locked server direction |
|---|---|---|
| C/C++ | C, C++, and their normal header/source variants | Use the discovered C/C++ language-server path; installation and version selection follow the common policy in §2. |
| ObjC/ObjCpp/Swift | Objective-C, Objective-C++, and Swift | **Apple: `sourcekit-lsp` is primary.** |
| PHP | PHP | Use the discovered PHP language-server path; installation and version selection follow the common policy in §2. |
| Vue | Vue single-file components | **Volar first.** |

The matrix is intentionally narrow. Other languages and alternate servers are not P0 commitments merely because the repository can recognize their file extensions or parsers.

## 2. Installation is session-gated

Language-server installation is an explicit, session-scoped operation:

1. Discovery checks the local environment first.
2. If the required server is absent, the default behavior is to show an installation prompt for the current session.
3. Installation proceeds only after that session permits it. There is no silent download or background install during startup, indexing, or an ordinary read-only query.
4. If the prompt is declined, unavailable, or the client is non-interactive, the server remains unavailable and the caller receives the normal unavailable/unknown result rather than a hidden fallback.

The prompt is therefore the default policy, not an opt-in safety feature. A caller may provide an explicit session policy for automation, but that policy must be visible at the session boundary and must not turn ordinary server discovery into an implicit install.

## 3. Artifact integrity and version selection

The install record uses the **official checksum published by the upstream release** for the selected artifact. The checksum is verified against the bytes installed locally; a locally computed hash is evidence for verification, not a replacement authority.

Version selection is:

- **Latest by default.** An unpinned request resolves the latest supported upstream release and verifies its official checksum before installation.
- **Version pin allowed.** A caller or project may request an explicit version. The pinned artifact must resolve to that version's official checksum; it must not silently float to latest.
- **No checksum bypass.** Missing, mismatched, or non-official checksum metadata fails installation rather than being accepted as an unverified binary.

A successful local install should retain enough metadata to identify the server, platform/artifact, resolved version, and verified checksum. Repeated sessions may reuse a verified local install without downloading it again.

## 4. Lifecycle and instruction contracts remain unchanged

P0 preserves the existing resource and prompt contracts:

- **L1:** connection/bootstrap instructions supplied at MCP initialization.
- **L2:** the full manual supplied through `initial_instructions`.
- **L3:** tool descriptions and schemas that constrain individual operations.
- **Idle TTL:** language-server pooling continues to reclaim an unused server after the existing idle TTL. The current default remains five minutes; this RFC does not change the TTL, memory limits, checkout protection, or retry/cooldown behavior.

Installation gating must not bypass these tiers, and starting a newly installed server must still go through the same pool and lifecycle rules as a server that was already present locally.

## 5. Vue sequencing

Volar is the first Vue implementation and the P0 default for Vue language intelligence. Vue support must not require a second language server in P0.

A dual-server arrangement (Volar plus a separate TypeScript server, with routing/coordination rules) is a **follow-up**. It may be designed and evaluated after the P0 path is stable, but it is not part of the P0 acceptance criteria and must not be introduced implicitly as a fallback.

## 6. Apple sequencing

For ObjC, ObjCpp, and Swift, `sourcekit-lsp` is the primary server direction. Apple-platform discovery and lifecycle work should optimize for that server first; an alternate implementation is not a P0 prerequisite.

## 7. Non-goals

This RFC does not:

- add languages outside C/C++, ObjC/ObjCpp/Swift, PHP, and Vue;
- authorize silent installation or an install-on-start default;
- make a locally generated checksum authoritative over the upstream checksum;
- change L1/L2/L3 instruction semantics or idle reclamation;
- promote Vue dual-server support into P0; or
- define a second Apple server before the `sourcekit-lsp` path is validated.

## 8. Post-P0 addition: Dart

### 8.1 Motivation

In multi-language projects containing Flutter/Dart (first observed in live sessions on the AI-con monorepo), an unsupported language presents a silent zero-hit ambiguity. When tools such as `search_code`, `find_symbol`, or `get_dependents` return zero matches or missing-target diagnostics without declaring language coverage boundaries, LLMs frequently misinterpret indexing absence as code non-existence, triggering circular tool retries, agent drift, and fallback to repetitive grep/read scans. Extending Astrolabe's deterministic code graph and precise language server dispatch to Dart closes this critical gap.

### 8.2 Key architectural decisions

#### 1. Tree-sitter parser selection and ABI verification
- **Decision:** Integrate `tree-sitter-dart 0.2` into the shared parsing pipeline.
- **Evidence:** Verified ABI compatibility with tree-sitter host 0.27 across 141 real-world Dart files with zero syntax `ERROR` nodes. Captures top-level and class-level symbols (`class`, `enum`, `mixin`, `function`, `getter`, `setter`), accurately reflects the leading underscore `_` naming convention for private visibility (`is_exported`), and captures import/export relations (`library_import`, `export`, `part`, `part of`).
- **Trade-off:** Avoids unmaintained parser variants and fulfills Astrolabe's zero-silent-failure contract.

#### 2. Line-based pubspec parsing over external YAML dependencies
- **Decision:** Implement dedicated line-level parsing for `pubspec.yaml` rather than introducing a heavyweight YAML parser crate.
- **Evidence:** Anchoring on column-0 `name:` tokens extracts the package name reliably without falling into nested `name:` fields inside dependency blocks. Maps package roots to `lib/`, resolves `package:<pkg>/...`, relative imports, and `part` directives, and returns `None` for external third-party packages and Dart SDK (`dart:`) libraries. Tested against the AI-con Flutter+Go monorepo: achieves 100% in-repo Dart import resolution across 204 `.dart` files and 1,900 symbols in 885ms with 93MB peak RSS. In multi-pubspec monorepos, duplicate package names resolve to the shallowest path. `.dart_tool/` directories are defended and excluded across three layers: scanner, watcher, and resolver.
- **Trade-off:** Provides deterministic in-repo topology at minimal runtime overhead without full YAML schema validation.

#### 3. LSP integration: `rootUri = null` and readiness status mapping
- **Decision:** Probe `dart language-server` bundled with the Dart/Flutter SDK (prompt-only, no binary download matrix needed), initialize with `rootUri = null` while preserving `workspaceFolders`, and map proprietary readiness notifications.
- **Evidence:** As documented in `oraios/serena#2045`, the Dart analysis server treats `rootUri` as a distinct analysis root in addition to `workspaceFolders` without deduplicating the two; in a monorepo, providing both causes duplicate full-tree analysis and continuous 100% CPU lockup. Astrolabe explicitly sets `rootUri` and `rootPath` to `null` while retaining `workspaceFolders`. Four initialization options (`onlyAnalyzeProjectsWithOpenFiles: false`, `closingLabels: false`, `outline: false`, `flutterOutline: false`) are injected to suppress auxiliary editor analysis tasks. Proprietary readiness notifications (`$/analyzerStatus` and `experimental/serverStatus`) are mapped to active progress tracking to prevent premature settle expirations on legacy analyzer versions (Dart 3.13 uses standard `$/progress`). Live verification confirms accurate definition jump and hover semantics.
- **Trade-off:** Relies on the user's existing SDK installation rather than automated distribution; overridable via `ASTROLABE_LSP_DART`.

#### 4. Anti-drift hook alignment with supported language set
- **Decision:** Narrow the PreToolUse whole-file read deny filter from the 58-extension superset to the 28 supported language extensions (`SUPPORTED_CODE_EXTENSIONS`).
- **Evidence:** Intercepting whole-file reads on unsupported file types (such as `.kt`, `.scala`, `.rb`) penalized agents without offering an Astrolabe symbolic alternative. The 58-extension catalog is retained as `KNOWN_CODE_EXTENSIONS` for unindexed language telemetry, and grep/mixed deny prompts append an explicit note that grep is legitimate for unindexed languages.
- **Trade-off:** Eliminates false-positive interruptions on unsupported languages while keeping strict guardrails on supported code.

#### 5. Storage backward compatibility: `Language::Dart (u8 = 14)`
- **Decision:** Assign `Language::Dart` to `u8 = 14`, appended to the end of the language byte enum.
- **Evidence:** Ensures strict binary backward compatibility for existing Redb storage layouts without requiring a schema version bump. Older Astrolabe binaries reading an index containing Dart files gracefully deserialize the language tag as `None` while keeping file paths and graph edges intact.
- **Trade-off:** Preserves index durability and avoids forced re-indexing across version transitions.

### 8.3 Known limitations and follow-ups

- **Call graph edges (`trace_calls`):** Call edge extraction for Dart returns empty in v1; AST-based call query analysis is reserved for a future release.
- **Build runner generated files:** Code generated by build runner (`.g.dart`, `.freezed.dart`) is currently indexed; glob-based exclusion support is planned as a follow-up.
- **Cold start settle window:** Cold start of `dart language-server` (~3.2s) slightly exceeds the uniform 3.0s LSP settle quiet window; initial queries are guarded by `didOpen` immediate priority analysis fallback.
- **Corpus edge case:** The `flutter/packages` corpus achieves a 99.66% import resolution rate due to multiple nested example packages sharing identical names; scheduled for corpus-specific resolver refinement.

