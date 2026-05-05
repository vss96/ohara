; Kotlin symbol extraction query (tree-sitter-kotlin-ng 1.1).
;
; The kotlin-ng grammar replaced `type_identifier` and `simple_identifier`
; with a single `identifier` node, and exposes a `name:` field on
; class_declaration / object_declaration / companion_object /
; function_declaration. Top-level declarations now sit under
; (source_file (statement (declaration ...))), and class-body members
; under (class_body (class_member_declaration (declaration ...))).
; `statement`, `declaration`, and `class_member_declaration` are
; supertype subtypes, so we navigate them with anonymous wildcards.
;
; sealed/data/inner/inline-value variants of class_declaration share
; the same AST node type, distinguished only by their `modifiers`
; child. Capturing the declaration is enough; modifiers stay inside
; the node's byte range (and therefore inside source_text), which is
; what the Spring-flavored tests rely on.

(class_declaration
  name: (identifier) @class_name) @def_class

(object_declaration
  name: (identifier) @class_name) @def_class

(companion_object
  name: (identifier) @class_name) @def_class

; Top-level function: in kotlin-ng, function_declaration appears as a
; direct child of source_file (the statement/declaration supertype
; nodes from node-types.json are flattened in the concrete tree).
(source_file
  (function_declaration
    name: (identifier) @func_name) @def_function)

; Member function: nested inside a class/object/companion body. The
; class_body wrapper is shared by class_declaration, object_declaration,
; and companion_object, so a single pattern covers all three contexts.
; class_member_declaration is similarly a flattened supertype here.
(class_body
  (function_declaration
    name: (identifier) @method_name) @def_method)
