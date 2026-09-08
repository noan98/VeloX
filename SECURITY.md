# Security Policy

## Supported Versions

Security fixes are provided for the latest release and the current `main` branch.
Older releases may not receive security fixes.

## Reporting a Vulnerability

Please do **not** open a public GitHub issue for a suspected security vulnerability.

Use the repository's **Security** tab and submit a private vulnerability report when available.
If private vulnerability reporting is not enabled, contact the repository owner through GitHub before disclosing the issue publicly.

When reporting, include:

- A clear description of the vulnerability
- Affected version or commit
- Reproduction steps or a minimal proof of concept
- Potential impact
- Any suggested mitigation, if known

Please allow reasonable time for investigation and remediation before public disclosure.

## Scope

Security reports for VeloX include, but are not limited to:

- Remote code execution or arbitrary code execution
- Privilege escalation
- Sandbox or process-isolation bypasses
- WebView2/browser security boundary bypasses
- Sensitive information disclosure
- Unsafe handling of downloaded or untrusted content
- Supply-chain vulnerabilities in the build and release process
- GitHub Actions or release workflow vulnerabilities

## Secrets and Sensitive Data

Never commit passwords, API keys, access tokens, signing credentials, private keys, certificates, or other secrets to this repository.

Release credentials and signing secrets must be stored in GitHub Actions secrets or environments, not in source control.
