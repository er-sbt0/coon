# Enhanced References with Symbol Names - Implementation Summary

This document summarizes the implementation of enhanced references with symbol names for the `coon` project.

## Overview

The enhanced references feature extends the existing reference tracking system to include information about the symbols that are making the references. This provides much richer context about how functions are being used throughout the codebase.

## Core Data Structures

### Enhanced Reference Types

```rust
/// Enhanced reference information that includes symbol context
pub struct Reference {
    pub location: Location,
    pub referencing_symbol: Option<ReferencingSymbol>,
}

/// Information about the symbol that is making a reference
pub struct ReferencingSymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: ReferenceSymbolKind,
}

/// Types of symbols that can make references
pub enum ReferenceSymbolKind {
    Function,
    Method,
    Constructor,
    Variable,
    Field,
    Class,
    Struct,
    Module,
    Unknown,
}
```

### Enhanced FunctionNode

The `FunctionNode` has been updated to support both legacy and enhanced references:

```rust
pub struct FunctionNode {
    pub id: SymbolId,
    pub name: String,
    pub qualified_name: String,
    pub definition_location: Location,
    pub references: Vec<Reference>, // Now enhanced references
    pub diagnostics: Vec<Diagnostic>,
}
```

New methods added:
- `add_reference_with_symbol()` - Add enhanced reference with symbol info
- `get_referencing_function_names()` - Get names of functions that reference this function
- `get_referencing_symbols_by_kind()` - Filter references by symbol type
- `get_reference_locations()` - Backward compatibility for location-only access

## LSP Integration

### Enhanced Client Methods

- `find_references_with_symbols()` - Enhanced reference request
- `enhance_references()` - Post-process references with symbol information

### Enhanced Response Types

```rust
pub struct EnhancedReferencesResponse {
    pub request_id: i64,
    pub references: Vec<core_data::Reference>,
}
```

### Symbol Resolution

New `symbol_resolution.rs` module provides utilities for:
- Finding containing symbols at specific positions
- Converting LSP symbol types to our internal types
- Mapping symbol information to references

## Service Layer

### Enhanced Service Methods

- `request_references_with_symbols()` - Public API for enhanced references
- `handle_references_with_symbols_request()` - Request handler
- Enhanced response processing with symbol information

### Response Types

```rust
pub enum LspResponse {
    // ... existing responses
    ReferencesWithSymbols {
        request_id: String,
        references: Vec<core_data::Reference>,
    },
}
```

## Usage Example

```rust
use core_data::{FunctionNode, Location, ReferencingSymbol, ReferenceSymbolKind};

// Create a function node
let mut target_function = FunctionNode::new(
    "calculate_sum".to_string(),
    "math::calculate_sum".to_string(),
    Location::new("math.c".to_string(), 10, 0),
);

// Add enhanced reference with symbol information
let caller_symbol = ReferencingSymbol {
    name: "main".to_string(),
    qualified_name: "main".to_string(),
    kind: ReferenceSymbolKind::Function,
};

target_function.add_reference_with_symbol(
    Location::new("main.c".to_string(), 25, 10),
    Some(caller_symbol),
);

// Query enhanced information
let function_callers = target_function.get_referencing_function_names();
println!("Functions calling calculate_sum: {:?}", function_callers);

// Get references by type
let function_refs = target_function.get_referencing_symbols_by_kind(ReferenceSymbolKind::Function);
println!("Function references: {}", function_refs.len());
```

## LSP Service Usage

```rust
use lsp_integration::{LspService, LspResponse};

// Request enhanced references
service.request_references_with_symbols(
    "req_001".to_string(),
    document_uri,
    position,
).await?;

// Handle the response
match response {
    LspResponse::ReferencesWithSymbols { request_id, references } => {
        for reference in references {
            if let Some(symbol) = &reference.referencing_symbol {
                println!("Reference from {} ({})", symbol.name, symbol.qualified_name);
            }
        }
    }
    _ => {}
}
```

## Backward Compatibility

The implementation maintains full backward compatibility:

1. Existing `add_reference()` method still works
2. Legacy `Vec<Location>` references are automatically converted to `Vec<Reference>` with `None` symbol info
3. `get_reference_locations()` provides access to just the locations for legacy code

## Testing

Comprehensive tests have been added:

### Core Data Tests
- `test_enhanced_reference_creation` - Basic reference creation
- `test_function_node_enhanced_references` - Function node enhancements

### LSP Integration Tests  
- `test_convert_lsp_symbol_kind` - Symbol type conversion
- `test_position_in_range` - Position containment logic

## Future Enhancements

1. **Full Symbol Resolution**: Complete the implementation in `enhance_references()` to actually resolve symbols using document symbols
2. **Caching**: Add intelligent caching of document symbols to avoid repeated LSP requests
3. **Cross-file Analysis**: Extend to track references across multiple files in a project
4. **Performance Optimization**: Batch symbol resolution requests for better performance

## File Structure

```
core_data/
├── src/
│   └── lib.rs                    # Enhanced data structures

lsp_integration/
├── src/
│   ├── lib.rs                    # Enhanced LSP client
│   ├── service.rs                # Enhanced service layer  
│   ├── symbol_resolution.rs      # Symbol resolution utilities
│   └── enhanced_references_tests.rs  # Tests
```

This implementation provides a solid foundation for rich reference analysis while maintaining compatibility with existing code.
