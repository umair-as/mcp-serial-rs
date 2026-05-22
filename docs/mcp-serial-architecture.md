# MCP Serial Server Architecture (ESP32-C6 Focus)

This document captures the high-level architecture of `mcp-serial-rs` and its current target workflow around `/dev/ttyUSB1` for ESP32-C6 Zephyr bring-up/automation.

## System Overview

```mermaid
flowchart LR
    A["MCP Client (Claude/Desktop/CLI)"] -->|MCP (JSON-RPC) over stdio| B["mcp-serial-rs Binary"]

    subgraph S["MCP Server (Rust)"]
      B --> C["main.rs\ntokio bootstrap; builds McpServer"]
      C --> D["rmcp SDK\ninitialize / tools/list / tools/call framing"]
      D --> E["mcp/mod.rs\nMcpServer: #[tool] handlers + journal hook"]
      E --> F["serial.list_ports"]
      E --> G["serial.open / close"]
      E --> H["serial.write / read / read_until / exec"]
      G --> I["serial/manager.rs\nSessionManager"]
      H --> I
      I --> J["serial/session.rs\nSession state + IO"]
      J --> K["tokio-serial backend"]
      H --> L["serial/parser.rs\nPattern matcher (regex)"]
      E --> M["config.rs\nallowlist, limits, defaults"]
      E --> N["errors.rs\ntyped errors -> JSON-RPC codes"]
      E --> R["mcp/journal.rs + serial/journal.rs\ntool-call JSONL audit journal"]
    end

    K --> O["/dev/ttyUSB1\nUSB-UART adapter"]
    O --> P["ESP32-C6 (Zephyr app)"]

    Q["tio /dev/ttyUSB1 (today)"] -. direct serial terminal .- O
    A -. replaces manual tio workflows with MCP tools .-> B
```

## Typical MCP Session

```mermaid
sequenceDiagram
    autonumber
    participant Client as MCP Client
    participant Server as mcp-serial-rs
    participant Sess as SessionManager
    participant UART as /dev/ttyUSB1
    participant ESP as ESP32-C6 (Zephyr)

    Client->>Server: initialize
    Server-->>Client: result {serverInfo, capabilities, protocolVersion}
    Client->>Server: notifications/initialized

    Client->>Server: tools/list
    Server-->>Client: result {tools: [serial.* + inputSchema]}

    Client->>Server: tools/call serial.open {port:"/dev/ttyUSB1", baud:115200}
    Server->>Sess: create session + validate allowlist
    Sess->>UART: open
    UART->>ESP: UART link up
    Server-->>Client: result.structuredContent {session_id}

    Client->>Server: tools/call serial.write {session_id, data:"help\\r\\n"}
    Server->>UART: write bytes
    UART->>ESP: command
    Server-->>Client: result.structuredContent {bytes_written}

    Client->>Server: tools/call serial.read_until {session_id, pattern:"READY|OK", timeout_ms:5000}
    ESP->>UART: console output
    UART->>Server: stream bytes
    Server->>Server: regex match in parser
    Server-->>Client: result.structuredContent {data, matched:true}

    Client->>Server: tools/call serial.close {session_id}
    Server->>Sess: close + remove session
    Server-->>Client: result.structuredContent {ok:true}
```

## Scope For Current Implementation

- Primary hardware path: ESP32-C6 over `/dev/ttyUSB1`.
- Primary objective: replace ad-hoc `tio` manual interaction with MCP-tool-driven, automatable serial workflows.
- MCP lifecycle (`initialize`, `tools/list`, `notifications/initialized`) and
  `tools/call` dispatch are handled by the `rmcp` SDK.
- Tools exposed via `tools/call`:
- `serial.list_ports`
- `serial.open`
- `serial.write`
- `serial.read`
- `serial.read_until`
- `serial.exec`
- `serial.close`
