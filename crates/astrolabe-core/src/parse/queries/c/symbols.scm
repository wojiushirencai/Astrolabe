;; C symbol definitions (tree-sitter-c 0.24).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Ported from OpenVisio + upstream tags.scm into Astrolabe capture vocabulary.
;; Structs use `@definition.struct` (Astrolabe has Struct); upstream tags used
;; class for structs.

;; -------------------------------------------------------------- functions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (identifier) @name))) @definition.function

(function_definition
  declarator: (pointer_declarator
    declarator: (pointer_declarator
      declarator: (function_declarator
        declarator: (identifier) @name)))) @definition.function

;; Prototypes / declarations (headers).
(declaration
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(declaration
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (identifier) @name))) @definition.function

;; ------------------------------------------------------------------ types
(struct_specifier
  name: (type_identifier) @name) @definition.struct

(union_specifier
  name: (type_identifier) @name) @definition.type

(enum_specifier
  name: (type_identifier) @name) @definition.enum

(type_definition
  declarator: (type_identifier) @name) @definition.type

;; --------------------------------------------------------------- constants
(enumerator
  name: (identifier) @name) @definition.const
