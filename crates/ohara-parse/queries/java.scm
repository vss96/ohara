; Java symbol extraction query (Plan 4).
; Captures top-level + nested type declarations and callable members.
; Sealed types appear as ordinary class_declaration / interface_declaration
; nodes (the `sealed` keyword is a modifier child); no separate AST node
; is required.

(class_declaration
  name: (identifier) @class_name) @def_class

(interface_declaration
  name: (identifier) @class_name) @def_class
