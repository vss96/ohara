(function_definition name: (identifier) @func_name) @def_function
(class_definition
  name: (identifier) @class_name
  body: (block
    (function_definition name: (identifier) @method_name) @def_method)) @def_class
