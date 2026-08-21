# Windows release setup

The Windows release is intentionally fail-closed. A release tag cannot publish Windows assets unless Azure Artifact Signing succeeds and every final signature validates as `Embrasure, Inc.` under the Microsoft Identity Verification Root Certificate Authority 2020.

## One-time setup

1. Accept the WiX 7 OSMF EULA and satisfy its maintenance-fee terms when applicable. The project records acceptance with `<AcceptEula>wix7</AcceptEula>`.
2. Create an Azure Artifact Signing account, complete public-trust identity validation for `Embrasure, Inc.`, and create a certificate profile.
3. Create an Azure workload identity for GitHub Actions. Its federated subject must be `repo:EmbrasureAI/embrasure-cli:environment:windows-signing`.
4. Grant only the Artifact Signing Certificate Profile Signer role on the certificate profile.
5. Create a protected GitHub environment named `windows-signing`. Restrict it to `v*.*.*` tags and require a maintainer reviewer.
6. Add these environment variables, not client secrets:
   - `AZURE_ARTIFACT_SIGNING_CLIENT_ID`
   - `AZURE_ARTIFACT_SIGNING_TENANT_ID`
   - `AZURE_ARTIFACT_SIGNING_SUBSCRIPTION_ID`
   - `AZURE_ARTIFACT_SIGNING_ENDPOINT`
   - `AZURE_ARTIFACT_SIGNING_ACCOUNT`
   - `AZURE_ARTIFACT_SIGNING_PROFILE`

The release workflow authenticates with GitHub OIDC. Only the protected signing job receives `id-token: write`.

## Release gate

Before the first public Windows release, test the signed assets on a clean Windows 11 x64 VM as a standard user with Smart App Control evaluation enabled and a real dbt project. Cover PowerShell 5.1 and 7 and the complete install → `embrasure doctor` → update → uninstall journey. Also test repair and downgrade rejection, verify that configuration survives uninstall, and confirm that no reboot or elevation is requested. Visually inspect the MSI at 100%, 125%, 150%, and 200% display scaling and retain the verbose installer log.

On Windows Server 2022+, test both policy states. The stock `DisableMSI=1` policy must reject the unmanaged per-user MSI with exit code 1625 and useful guidance. A standard user must pass the complete lifecycle after an administrator sets **Turn off Windows Installer** to **Never** (`DisableMSI=0`). The installer must never change this machine policy itself.

Azure Artifact Signing establishes publisher identity but does not guarantee immediate SmartScreen reputation. Never substitute an unsigned or self-signed public artifact while reputation develops.
