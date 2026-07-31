# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub Security Advisories for `jazzonaut/agentbench`. Do not open a public issue for unpatched vulnerabilities or include credentials, proprietary source code, prompts, or report files containing sensitive output.

## Data handling

AgentBench stores reports locally and does not upload telemetry. Paths, arguments, environment values, prompts, and command output are redacted by default. The optional `--save-command-output` flag deliberately persists a bounded output tail; review that report before sharing it.
