;; C++ symbol definitions (tree-sitter-cpp 0.23).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; Ported from OpenVisio + upstream tags.scm. Classes use Class; structs use
;; Struct; namespaces map to Module.

;; ------------------------------------------------------------ namespaces
(namespace_definition
  name: (namespace_identifier) @name) @definition.module

;; -------------------------------------------------------------- functions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (identifier) @name))) @definition.function

(function_definition
  declarator: (reference_declarator
    (function_declarator
      declarator: (identifier) @name))) @definition.function

;; Methods / out-of-line definitions: `ns::Cls::method` or `Cls::method`.
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier
      name: (identifier) @name))) @definition.method

(function_definition
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @definition.method

(function_definition
  declarator: (pointer_declarator
    declarator: (function_declarator
      declarator: (qualified_identifier
        name: (identifier) @name)))) @definition.method

;; Declarations / prototypes.
(declaration
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.function

(declaration
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @definition.method

(declaration
  declarator: (function_declarator
    declarator: (qualified_identifier
      name: (identifier) @name))) @definition.method

;; Inside class / struct bodies, bare field_identifier methods.
(field_declaration
  declarator: (function_declarator
    declarator: (field_identifier) @name)) @definition.method

(field_declaration
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition.method

;; ------------------------------------------------------------------ types
(class_specifier
  name: (type_identifier) @name) @definition.class

(struct_specifier
  name: (type_identifier) @name) @definition.struct

(union_specifier
  name: (type_identifier) @name) @definition.type

(enum_specifier
  name: (type_identifier) @name) @definition.enum

(type_definition
  declarator: (type_identifier) @name) @definition.type

;; Template class / struct (name lives on the inner specifier).
(template_declaration
  (class_specifier
    name: (type_identifier) @name)) @definition.class

(template_declaration
  (struct_specifier
    name: (type_identifier) @name)) @definition.struct

;; ----------------------------------------------------------------- fields
(field_declaration
  declarator: (field_identifier) @name) @definition.field

(field_declaration
  declarator: (pointer_declarator
    declarator: (field_identifier) @name)) @definition.field

;; --------------------------------------------------------------- constants
(enumerator
  name: (identifier) @name) @definition.const
