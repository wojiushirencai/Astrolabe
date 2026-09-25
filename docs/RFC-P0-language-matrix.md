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
