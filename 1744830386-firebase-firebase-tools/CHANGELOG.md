- Fixed an issue in the extensions emulator where parameter default values would not be substitued into resource definitions.
- Keep artifact registry dry run off for policy changes (#8419)
- Allowed users to create paid Cloud SQL instances for Data Connect when the free trial has already been used.
- Updated the Firebase Data Connect local toolkit to v2.2.0, which contains the following changes: (#8434)
  - Added support for aggregate functions on singular fields.
  - Added the ability to get an operation name without calling the `ref` function in generated web SDK.
  - Properly enforced one-of validation on `inc`, `dec`, `append`, and `prepend` update transforms. Existing deployed connectors that violate this constraint will still work, but will need to be fixed to use list syntax before being re-deployed.
  - Fixed an issue so that when using mutations with no variables, correct types are passed in.
