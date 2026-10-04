;; C# symbol definitions (tree-sitter-c-sharp 0.23).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Records are classes (a `record struct` is still reported as a class; there
;; is no separate symbol kind). Properties are type members (`field`).
;; Constructors are methods, matching the Java query pack. Namespaces,
;; including file-scoped `namespace N;`, are modules.

(class_declaration
  name: (identifier) @name) @definition.class

(struct_declaration
  name: (identifier) @name) @definition.struct

(interface_declaration
  name: (identifier) @name) @definition.interface

(enum_declaration
  name: (identifier) @name) @definition.enum

(record_declaration
  name: (identifier) @name) @definition.class

(method_declaration
  name: (identifier) @name) @definition.method

(constructor_declaration
  name: (identifier) @name) @definition.method

(property_declaration
  name: (identifier) @name) @definition.field

(namespace_declaration
  name: (_) @name) @definition.module

(file_scoped_namespace_declaration
  name: (_) @name) @definition.module

;; Top-level local functions (C# scripts and file-level statements).
;; The grammar already has `local_function_statement` under `global_statement`.
;; Nested local functions inside methods are not matched.
(compilation_unit
  (global_statement
    (local_function_statement
      name: (identifier) @name) @definition.function))
