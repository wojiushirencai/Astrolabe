(class_declaration name: (identifier) @name) @definition.class
(enum_declaration name: (identifier) @name) @definition.enum
(mixin_declaration name: (identifier) @name) @definition.trait
(function_signature name: (identifier) @name) @definition.function
(getter_signature name: (identifier) @name) @definition.method
(setter_signature name: (identifier) @name) @definition.method
;; Named constructors (`Foo.bar(...)`, incl. `const` / `factory`) are
;; indexed as `bar`. Unnamed constructors share the class symbol's name and
;; are already found through it.
(constructor_signature (identifier) "." (identifier) @name . parameters: (formal_parameter_list)) @definition.method
(constant_constructor_signature (identifier) "." (identifier) @name . parameters: (formal_parameter_list)) @definition.method
(factory_constructor_signature (identifier) "." (identifier) @name . parameters: (formal_parameter_list)) @definition.method
(redirecting_factory_constructor_signature (identifier) "." (identifier) @name . parameters: (formal_parameter_list)) @definition.method
