;; PHP import / include specifiers (tree-sitter-php 0.24).
;;
;; `@import` — `use` clause path (namespace\Name) or include/require path.
;; Quotes on string includes are stripped by `extract_imports` via `unquote`.
;; `namespace_use` is captured for graph display; filesystem resolution of
;; `use` is not attempted (same narrow policy as OpenVisio).

(namespace_use_clause
  [
    (name) @import
    (qualified_name) @import
  ])

(include_expression
  (string) @import)

(include_once_expression
  (string) @import)

(require_expression
  (string) @import)

(require_once_expression
  (string) @import)
