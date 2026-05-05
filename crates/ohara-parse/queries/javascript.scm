(function_declaration name: (identifier) @func_name) @def_function

; Capture class declarations independently of body contents so empty
; classes and field-only classes still produce a Class symbol. Methods
; are captured by the separate pattern below.
(class_declaration name: (identifier) @class_name) @def_class

(class_declaration
  body: (class_body
    (method_definition name: (property_identifier) @method_name) @def_method))

(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (arrow_function))) @def_arrow

(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (function_expression))) @def_arrow
