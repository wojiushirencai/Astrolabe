;; C include specifiers (tree-sitter-c 0.24).
;;
;; `@import` — path text. Quotes / angle brackets are stripped by
;; `extract_imports` via `unquote`. Angle (system) includes stay in the list
;; for display; filesystem resolution only attempts quoted includes.

(preproc_include
  path: (string_literal) @import)

(preproc_include
  path: (system_lib_string) @import)
