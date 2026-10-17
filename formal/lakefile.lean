import Lake
open Lake DSL
package ishSemantics
lean_lib IshSemantics
lean_exe protocolTests where
  root := `ProtocolTests
lean_exe semanticBridge where
  root := `Main
