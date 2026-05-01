; Kotlin symbol extraction query (Plan 4).
;
; Kotlin grammar 0.3.x does not expose a `name:` field on class /
; object / companion declarations — the identifier is just a child of
; type_identifier kind. Captures rely on positional patterns instead.
;
; sealed/data/inner/inline-value variants of class_declaration share
; the same AST node type, distinguished only by their `modifiers`
; child. Capturing the declaration is enough; modifiers stay inside
; the node's byte range (and therefore inside source_text), which is
; what the Spring-flavored tests rely on.

(class_declaration
  (type_identifier) @class_name) @def_class

(object_declaration
  (type_identifier) @class_name) @def_class

(companion_object
  (type_identifier) @class_name) @def_class

; Top-level function: directly under source_file.
(source_file
  (function_declaration
    (simple_identifier) @func_name) @def_function)

; Member function: nested inside a class/object/companion body. The
; class_body wrapper is shared by class_declaration, object_declaration,
; and companion_object, so a single pattern covers all three contexts.
(class_body
  (function_declaration
    (simple_identifier) @method_name) @def_method)
