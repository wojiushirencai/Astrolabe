;; TypeScript / TSX symbol definitions (tree-sitter-typescript 0.23; the
;; `typescript` and `tsx` grammars share every node type used here).
;;
;; `@definition.<kind>` on the defining node, `@name` on its identifier.
;; `export` / `export default` / `declare` are wrappers around the declaration
;; node, so none of the patterns below need to mention them — the inner
;; declaration matches wherever it sits. Consumers can check whether the
;; definition's parent is an `export_statement` to fill `exported`.

;; -------------------------------------------------------------- functions
(function_declaration
  name: (identifier) @name) @definition.function

(generator_function_declaration
  name: (identifier) @name) @definition.function

;; Overload signatures and `declare function f(): void;`
(function_signature
  name: (identifier) @name) @definition.function

;; const / let / var f = () => {} | function () {} | function* () {}
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression) (generator_function)]) @definition.function)

(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression) (generator_function)]) @definition.function)

;; module.exports.f = function () {} / Foo.prototype.bar = () => {} / f = () => {}
(assignment_expression
  left: [
    (identifier) @name
    (member_expression
      property: (property_identifier) @name)
  ]
  right: [(arrow_function) (function_expression) (generator_function)]) @definition.function

;; { handler: () => {} } object-literal function properties.
(pair
  key: (property_identifier) @name
  value: [(arrow_function) (function_expression) (generator_function)]) @definition.function

;; `export default function () {}` / `export default () => {}` — anonymous, so
;; the `default` keyword itself is the name, which is what tsserver reports.
(export_statement
  "default" @name
  value: [
    (function_expression !name)
    (generator_function !name)
    (arrow_function)
  ] @definition.function)

;; ---------------------------------------------------------------- classes
(class_declaration
  name: (type_identifier) @name) @definition.class

(abstract_class_declaration
  name: (type_identifier) @name) @definition.class

;; Named class expression: `const X = class Named {}` reports `Named`.
(class
  name: (type_identifier) @name) @definition.class

;; Anonymous class expression bound to a name: `const Anon = class {}`.
(variable_declarator
  name: (identifier) @name
  value: (class !name)) @definition.class

;; `export default class {}`
(export_statement
  "default" @name
  value: (class !name) @definition.class)

;; ---------------------------------------------------------------- methods
;; Class and object-literal methods, getters/setters, `constructor`,
;; `#private()`.
(method_definition
  name: [(property_identifier) (private_property_identifier)] @name) @definition.method

;; Interface members and overload signatures inside classes.
(method_signature
  name: [(property_identifier) (private_property_identifier)] @name) @definition.method

(abstract_method_signature
  name: [(property_identifier) (private_property_identifier)] @name) @definition.method

;; `class A { onClick = (e) => {} }`
(public_field_definition
  name: [(property_identifier) (private_property_identifier)] @name
  value: [(arrow_function) (function_expression)]) @definition.method

;; ------------------------------------------------------------------ types
(interface_declaration
  name: (type_identifier) @name) @definition.interface

(type_alias_declaration
  name: (type_identifier) @name) @definition.type

(enum_declaration
  name: (identifier) @name) @definition.enum

;; `namespace A.B {}` / `declare module 'x' {}` (string name without quotes).
(internal_module
  name: [
    (identifier) @name
    (nested_identifier) @name
    (string (string_fragment) @name)
  ]) @definition.module

(module
  name: [
    (identifier) @name
    (nested_identifier) @name
    (string (string_fragment) @name)
  ]) @definition.module

;; -------------------------------------------------------------- constants
;; Module-level `const` bindings whose value is *not* a function or class
;; (those are reported above). The value list is an explicit allow-list so the
;; two sets never overlap. `let` / `var` and locals are skipped.
(program
  (lexical_declaration
    kind: "const"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression) (as_expression) (satisfies_expression)
        (non_null_expression)
      ]) @definition.const))

(export_statement
  declaration: (lexical_declaration
    kind: "const"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression) (as_expression) (satisfies_expression)
        (non_null_expression)
      ]) @definition.const))

;; `declare const X: T;` — no value, only a type annotation.
(ambient_declaration
  (lexical_declaration
    kind: "const"
    (variable_declarator
      name: (identifier) @name
      !value) @definition.const))

;; -------------------------------------------------------------- variables
;; Module-level `let` / `var` whose value is not a function or class (those
;; are already `definition.function` / `definition.class`). Same allow-list
;; as Const so the two sets never overlap with methods-as-arrow-fields.
(program
  (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(program
  (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression) (as_expression) (satisfies_expression)
        (non_null_expression)
      ]) @definition.variable))

(export_statement
  declaration: (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(export_statement
  declaration: (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression) (as_expression) (satisfies_expression)
        (non_null_expression)
      ]) @definition.variable))

(program
  (variable_declaration
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(program
  (variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression) (as_expression) (satisfies_expression)
        (non_null_expression)
      ]) @definition.variable))

(export_statement
  declaration: (variable_declaration
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(export_statement
  declaration: (variable_declaration
    (variable_declarator
      name: (identifier) @name
      value: [
        (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
        (object) (array) (identifier) (member_expression) (subscript_expression)
        (call_expression) (new_expression) (await_expression)
        (binary_expression) (unary_expression) (ternary_expression)
        (parenthesized_expression) (as_expression) (satisfies_expression)
        (non_null_expression)
      ]) @definition.variable))

(ambient_declaration
  (lexical_declaration
    kind: "let"
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

(ambient_declaration
  (variable_declaration
    (variable_declarator
      name: (identifier) @name
      !value) @definition.variable))

;; ----------------------------------------------------------------- fields
;; Interface property signatures. Method signatures (`onSave(): void`) are
;; already `@definition.method`. Restricted to `interface_body` so inline
;; object types (`({ label }: { label: string })`) are not reported.
(interface_declaration
  body: (interface_body
    (property_signature
      name: [(property_identifier) (private_property_identifier)] @name) @definition.field))

;; Class fields that are not methods. `onClick = (e) => {}` is captured as
;; a method above via an explicit function-value list; this allow-list is
;; the complement, plus fields with only a type (`x: number`).
(public_field_definition
  name: [(property_identifier) (private_property_identifier)] @name
  !value) @definition.field

(public_field_definition
  name: [(property_identifier) (private_property_identifier)] @name
  value: [
    (number) (string) (template_string) (regex) (true) (false) (null) (undefined)
    (object) (array) (identifier) (member_expression) (subscript_expression)
    (call_expression) (new_expression) (await_expression)
    (binary_expression) (unary_expression) (ternary_expression)
    (parenthesized_expression) (as_expression) (satisfies_expression)
    (non_null_expression) (this) (super)
  ]) @definition.field
