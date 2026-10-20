# Rust Development Rules

When adding a Rust module or new Rust functionality, follow this sequence:

1. **Describe the interface first.** State the module's public types and functions, their inputs and outputs, ownership and borrowing expectations, and observable behavior. Keep this description focused on the contract, not its implementation.
2. **Write unit tests for that interface.** Tests must exercise the described behavior through the intended interface, including relevant success cases and failure or boundary cases. Do not implement the functionality before these tests are in place.
3. **Implement the code.** Make the tests pass while keeping the implementation aligned with the described contract. If implementation reveals that the interface or expected behavior must change, update the description and tests before proceeding.

Do not skip this sequence for small additions. For changes that cannot reasonably be unit tested (for example, hardware-specific integration), explain why and add the closest meaningful test or verification available.
