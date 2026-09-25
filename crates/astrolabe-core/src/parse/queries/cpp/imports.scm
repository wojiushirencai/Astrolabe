;; C++ include specifiers (tree-sitter-cpp 0.23).
;;
;; Same shape as C: quoted and angle includes. Quotes / brackets stripped by
;; `extract_imports` via `unquote`.

(preproc_include
  path: (string_literal) @import)

(preproc_include
  path: (system_lib_string) @import)
