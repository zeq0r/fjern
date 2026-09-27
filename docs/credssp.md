# CredSSP implementation status

The client implements an experimental NTLM CredSSP exchange over verified TLS.
`nla-probe` stops after the server challenge; `login` attempts authentication,
TLS binding and credential delegation once. Neither starts a desktop session.

## Authentication boundary

Fjern owns RDP negotiation, TSRequest framing, phase validation, TLS trust,
public-key binding, credential encoding and transport deadlines. The MIT/Apache
licensed `sspi-rs` provider handles NTLMv2 tokens and signing/sealing. It does not
replace Fjern's RDP engine or CredSSP state machine.

This first path uses raw NTLM tokens, permitted by MS-CSSP. It does not implement
SPNEGO negotiation, Kerberos, UPN accounts, smartcards, Entra ID, Remote Credential
Guard or restricted-admin mode. Use `username` or `DOMAIN\username`; for an
explicit local account use `MACHINE\username`. NTLM-disabled hosts are unsupported.

## Exchange

1. Complete TLS with the selected trust policy. The CredSSP diagnostics reject
   TLS-only negotiation before collecting credentials; interactive `connect`
   uses a separate TLS-only Client Info path when the server selects it.
2. For `login`, prompt for a password on the local terminal with echo disabled.
   No password argument, environment variable or saved-password file is used.
3. Send the provider's Type 1 token in a version 6 TSRequest. It has no username,
   password or workstation identity. Receive one Type 2 challenge, require
   CredSSP version 5+ and signing, sealing, extended-session security, target
   information, 128-bit security and key exchange. `nla-probe` stops here.
4. Generate the Type 3 response and directional NTLM protection context. Send
   the final token together with the sealed client TLS-binding hash and fresh
   32-byte nonce. NTLM authentication data is sent at this step; it is distinct
   from the later password-credential delegation.
5. Require a valid server binding response, verify its NTLM signature/sequence
   number and compare the directional SHA-256 binding hash in constant time.
   Errors are terminal. Credentials cannot be delegated before this succeeds.
6. Encode TSPasswordCreds inside TSCredentials with UTF-16LE fields, seal them
   and send only authInfo. If HYBRID_EX was selected, read the four-byte early
   authorization result. With HYBRID alone, report delegation without claiming
   confirmed authorization. `login` closes the connection here. `session-probe`
   continues with [MCS/GCC settings and channel setup](mcs.md). `connect` also
   retains the identity long enough to encode Client Info in zeroizing storage,
   sends it under verified TLS and releases it before the display loop.

## Bounds and secrets

DER decoding rejects duplicate, unordered and unknown fields. The local message
limit is 1 MiB with at most 16 tokens, while this NTLM flow requires one challenge
token. TLS plaintext framing reads exactly one message and preserves subsequent
messages. Each network read/write phase has a five-second deadline. Password
entry and local cryptographic computation are outside those deadlines. No
application-level automatic authentication retries occur.

Password/credential buffers owned by Fjern use `Secret` or `Zeroizing`, and
credential DER is encoded into preallocated zeroizing buffers. No diagnostic
logging subscriber is installed: provider trace events can contain sensitive
buffers and must not be enabled without a separate redaction review. This is
not a guarantee that every temporary allocation inside dependencies is erased.

The NTLM adapter permanently fails after sealing/unsealing errors, verifies
integrity before returning plaintext and relies on the provider for directional
keys and sequence numbers. Pins and certificates follow [TLS trust](tls.md).

## Validation and limits

Loopback tests use real TLS, real NTLM client/server contexts and synthetic
credentials. They cover successful binding/delegation/early authorization,
wrong passwords, altered bindings, authorization denial, message tampering,
replay, reflection and probing without credentials. Additional wire vectors
cover DER and Unicode credential encoding. Test fixtures are not an OS login.

The Windows host answered the real NLA probe with CredSSP version 6 and an NTLM
challenge. Real Windows NTLM authentication, binding and early authorization
have since passed with a standard test account. Linux interoperability remains
unverified. See the [host report](windows-first-probe.md).

## References

- [TSRequest, MS-CSSP 2.2.1](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cssp/6aac4dea-08ef-47a6-8747-22ea7f6d8685)
- [TSPasswordCreds](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cssp/17773cc4-21e9-4a75-a0dd-72706b174fe5)
- [CredSSP sequencing](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cssp/385a7489-d46b-464c-b224-f7340e308a5c)
- [Early authorization](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/d0e560a3-25cb-4563-8bdc-6c4cc625bbfc)
- [sspi-rs](https://github.com/Devolutions/sspi-rs)
