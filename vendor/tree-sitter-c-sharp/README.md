# C# grammar

Based on tree-sitter-c-sharp 0.23.5, upstream commit
`cac6d5fb595f5811a076336682d5d595ac1c9e85`:
https://github.com/tree-sitter/tree-sitter-c-sharp/tree/v0.23.5

Local grammar corrections:

- Allow a pointer's operand to be an expression, with unary precedence.
  This covers postfix increments, casts, and pointer arithmetic.
- Classify invocations as possible lvalues because a method can return a ref.
  Preserve invocations as expression statements.
- Declare the assignment/expression conflict introduced by pointer operands.
- Explicitly discard the scanner's popped value with the generator's C headers.

`grammar.js` is the grammar source. `src/grammar.json` is its generated form.
After editing the grammar, regenerate the JSON with tree-sitter 0.26.8:

```sh
tree-sitter generate --abi 15 --js-runtime native --no-parser
```

The build uses the pinned official Rust generator to produce the parser,
node types, and C headers in Cargo's output directory. It needs no JavaScript
runtime or installed tree-sitter CLI. Generated parser C is not committed.
The scanner, queries, Rust API, and MIT notice are retained from upstream.
