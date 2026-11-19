# Test-only TLS identity

These fixtures are used only by local relay pinning tests. The CA is not trusted by any production client. The committed private key is a deliberately public test key and must never be used by a deployed service.

The P-256 CA and endpoint certificates were generated with Python cryptography. The endpoint covers localhost and 127.0.0.1. The valid certificate expires in 2120; expired.pem uses the same endpoint public key but expired in 2001. This verifies that a matching pin cannot bypass expiry checks. public-key.pem is the endpoint SubjectPublicKeyInfo used by the CLI import tests.
