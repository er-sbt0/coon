use core_data::{Location, Reference, ReferenceSymbolKind, ReferencingSymbol};

#[test]
fn test_enhanced_reference_creation() {
    let location = Location::new("test.c".to_string(), 10, 5);

    // Test simple reference (backward compatibility)
    let simple_ref = Reference {
        location: location.clone(),
        referencing_symbol: None,
    };

    assert_eq!(simple_ref.location.file_path, "test.c");
    assert!(simple_ref.referencing_symbol.is_none());

    // Test enhanced reference with symbol information
    let referencing_symbol = ReferencingSymbol {
        name: "main".to_string(),
        qualified_name: "main".to_string(),
        kind: ReferenceSymbolKind::Function,
    };

    let enhanced_ref = Reference {
        location: location.clone(),
        referencing_symbol: Some(referencing_symbol),
    };

    assert_eq!(enhanced_ref.location.file_path, "test.c");
    assert!(enhanced_ref.referencing_symbol.is_some());

    let symbol = enhanced_ref.referencing_symbol.unwrap();
    assert_eq!(symbol.name, "main");
    assert_eq!(symbol.qualified_name, "main");
    assert_eq!(symbol.kind, ReferenceSymbolKind::Function);
}

#[test]
fn test_function_node_enhanced_references() {
    use core_data::FunctionNode;

    let mut function = FunctionNode::new(
        "target_function".to_string(),
        "my_module::target_function".to_string(),
        Location::new("target.c".to_string(), 5, 0),
    );

    // Add a simple reference
    function.add_reference(Location::new("caller1.c".to_string(), 10, 5));

    // Add enhanced references
    let caller_function = ReferencingSymbol {
        name: "caller_func".to_string(),
        qualified_name: "caller_func".to_string(),
        kind: ReferenceSymbolKind::Function,
    };

    function.add_reference_with_symbol(
        Location::new("caller2.c".to_string(), 15, 10),
        Some(caller_function),
    );

    let variable_ref = ReferencingSymbol {
        name: "func_ptr".to_string(),
        qualified_name: "func_ptr".to_string(),
        kind: ReferenceSymbolKind::Variable,
    };

    function.add_reference_with_symbol(
        Location::new("caller3.c".to_string(), 20, 15),
        Some(variable_ref),
    );

    // Test the enhanced functionality
    assert_eq!(function.references.len(), 3);
    assert_eq!(function.get_reference_locations().len(), 3);

    let function_references = function.get_referencing_function_names();
    assert_eq!(function_references.len(), 1);
    assert_eq!(function_references[0], "caller_func");

    let variable_symbols = function.get_referencing_symbols_by_kind(ReferenceSymbolKind::Variable);
    assert_eq!(variable_symbols.len(), 1);
    assert_eq!(variable_symbols[0].name, "func_ptr");

    let function_symbols = function.get_referencing_symbols_by_kind(ReferenceSymbolKind::Function);
    assert_eq!(function_symbols.len(), 1);
    assert_eq!(function_symbols[0].name, "caller_func");
}
