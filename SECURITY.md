# Security Policy

## Supported Versions

Rayline is under active pre-release development. Only the latest release and
`main` are supported; security fixes are applied there.

## Reporting a Vulnerability

Please do not open public issues for security vulnerabilities. Public reports
can expose users before a fix is available.

Report privately using GitHub Private Vulnerability Reporting:

1. Go to the Security tab of this repository.
2. Click Report a vulnerability.
3. Provide a clear description, affected version, and reproduction steps.

If you cannot use GitHub Private Vulnerability Reporting, email
[security@rayline.ai](mailto:security@rayline.ai).

When reporting, please include:

- the affected version and operating system;
- a description of the issue and its impact;
- step-by-step reproduction;
- any relevant logs with secrets and tokens removed.

## What to Expect

We aim to acknowledge reports promptly, confirm and assess the issue, and keep
you informed as we work on a fix and coordinate disclosure.

## Local TLS Interception

Rayline can install a local certificate authority and intercept TLS traffic on
your own machine to route requests. This is core to how proxy-mode routing
works and is documented in the [README](README.md#disclaimers).

Reports about this intended behavior are not security vulnerabilities. Reports
about flaws in this mechanism, such as certificate handling, key storage, or
privilege escalation, should be reported through the channels above.
