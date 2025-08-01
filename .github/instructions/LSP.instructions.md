---
applyTo: '**'
---
description: >
  Use these guidelines to assist developers in implementing or interacting with
  Language Server Protocol version 3.17 (LSP 3.17). Focus on message formats,
  capabilities negotiation, and new features.

intents:
  - question: "What is LSP 3.17?"
    answer: |
      LSP 3.17 is the latest version of the open, JSON‑RPC‑based protocol used
      between editors (clients) and language servers to implement features such
      as completion, hover, diagnostics, inline hints, and more :contentReference[oaicite:1]{index=1}.

  - question: "What’s new in 3.17?"
    answer: |
      Version 3.17 introduces:
        • Type hierarchy support  
        • Inline values and inlay hints  
        • Notebook document support  
        • A built‑in meta‑model for generating SDKs and types :contentReference[oaicite:2]{index=2}.

  - question: "How does LSP messaging work?"
    answer: |
      LSP uses JSON‑RPC 2.0 messages wrapped in an HTTP‑style header (Content‑Length and optional
      Content‑Type), with ASCII headers, then a JSON body encoded in UTF‑8. Clients and servers
      exchange requests, responses, and notifications encoding editing operations and tool actions :contentReference[oaicite:3]{index=3}.

  - question: "How are client and server capabilities negotiated?"
    answer: |
      During initialization, the client sends `initialize` with declared capabilities,
      and the server responds with supported capabilities. Some features may be
      registered dynamically later. This “capability flags” mechanism enables
      backward compatibility and optional feature negotiation :contentReference[oaicite:4]{index=4}.

  - question: "How is text synchronization handled?"
    answer: |
      The client sends `textDocument/didOpen`, `didChange`, and `didClose` notifications
      to synchronize document content. Change notifications keep the server and client
      in sync as the user types :contentReference[oaicite:5]{index=5}.

  - question: "What transport mechanisms are supported?"
    answer: |
      LSP can operate over stdin/stdout, sockets, or pipes. Typically the client launches
      the server process and communicates over standard I/O. It's not an HTTP server,
      but a simple bidirectional JSON‑RPC transport :contentReference[oaicite:6]{index=6}.

  - question: "What features can a language server implement?"
    answer: |
      Standard language features include:
        • auto‑completion (`textDocument/completion`)  
        • hover info (`textDocument/hover`)  
        • signature help, formatting, code actions, go‑to‑definition  
      Plus newer additions like type hierarchy, inline values, inlay hints, notebook support :contentReference[oaicite:7]{index=7}.

  - question: "How do I implement a simple LSP server?"
    answer: |
      Typically use an SDK: e.g. `vscode‑languageserver` (Node.js), `pygls` (Python), `tower‑lsp` (Rust),
      or `LSP4J` (Java). These frameworks encapsulate the JSON‑RPC loop, headers, capabilities,
      message dispatch, and lifecycle events like init, shutdown, request/notification handling :contentReference[oaicite:8]{index=8}.

metadata:
  spec_version: "3.17"
  base_protocol: "JSON‑RPC 2.0 over Content‑Length headers"
  new_features:
    - type-hierarchy
    - inlay-hints
    - inline-values
    - notebook-document
    - meta-model
