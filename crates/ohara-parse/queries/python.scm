(function_definition name: (identifier) @func_name) @def_function

; Capture class definitions independently of body contents so empty
; classes (`class Foo: pass`) and field-only classes still produce a
; Class symbol. Methods are captured by the separate pattern below.
(class_definition name: (identifier) @class_name) @def_class

(class_definition
  body: (block
    (function_definition name: (identifier) @method_name) @def_method))
