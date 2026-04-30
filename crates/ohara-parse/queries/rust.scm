(function_item name: (identifier) @name) @def_function
(impl_item type: (type_identifier) @impl_type
  body: (declaration_list (function_item name: (identifier) @method_name) @def_method))
(struct_item name: (type_identifier) @struct_name) @def_struct
(enum_item name: (type_identifier) @enum_name) @def_enum
