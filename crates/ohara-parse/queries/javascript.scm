(function_declaration name: (identifier) @func_name) @def_function

(class_declaration
  name: (identifier) @class_name
  body: (class_body
    (method_definition name: (property_identifier) @method_name) @def_method)) @def_class
