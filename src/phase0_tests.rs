mod mcp_contract {
    include!("phase0_mcp_tests.rs");
}

mod mcp_io_contract {
    include!("phase0_mcp_io_tests.rs");
}

mod execution_contract {
    include!("phase0_execution_tests.rs");
}

mod tool_validation_contract {
    include!("phase0_tool_validation_tests.rs");
}

mod approval_contract {
    include!("phase0_approval_tests.rs");
}

mod sandbox_contract {
    include!("phase0_sandbox_tests.rs");
}
