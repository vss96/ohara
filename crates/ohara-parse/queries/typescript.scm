(function_declaration name: (identifier) @func_name) @def_function

(class_declaration
  name: (type_identifier) @class_name
  body: (class_body
    (method_definition name: (property_identifier) @method_name) @def_method)) @def_class

(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (arrow_function))) @def_arrow

(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow_name
    value: (function_expression))) @def_arrow

(interface_declaration name: (type_identifier) @class_name) @def_class
(type_alias_declaration name: (type_identifier) @class_name) @def_class
(enum_declaration name: (identifier) @class_name) @def_class
