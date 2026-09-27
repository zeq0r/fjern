//! A bounded, one-attempt NTLM CredSSP exchange over previously verified TLS.
use crate::{credentials, ntlm, tls};
use linrdp_proto::{binding::BindingClient, credssp::TsRequest, negotiation::SecurityProtocol};
use sspi::{AuthIdentity, Username};
use std::net::TcpStream;

type Error = Box<dyn std::error::Error>;

pub fn account(value: &str) -> Result<(&str, &str), Error> {
    if value.contains('@') || value.contains('\0') || value.len() > 65536 {
        return Err(
            "use a local username or DOMAIN\\username; UPN/Kerberos is not supported yet".into(),
        );
    }
    let (domain, user) = value.split_once('\\').unwrap_or(("", value));
    if user.is_empty() || user.contains('\\') || (value.contains('\\') && domain.is_empty()) {
        return Err("expected username or DOMAIN\\username".into());
    }
    Ok((domain, user))
}

pub fn run(
    connection: &mut rustls::ClientConnection,
    stream: &mut TcpStream,
    host: &str,
    protocol: SecurityProtocol,
    user: Option<&str>,
) -> Result<Option<AuthIdentity>, Error> {
    if protocol == SecurityProtocol::Tls {
        return Err("server selected TLS-only security; NLA was not negotiated".into());
    }
    let identity = user.map(prompt_identity).transpose()?;
    exchange(connection, stream, host, protocol, user, identity.as_ref())?;
    Ok(identity)
}

pub fn prompt_identity(value: &str) -> Result<AuthIdentity, Error> {
    let (domain, user) = account(value)?;
    let username = Username::new(
        user,
        if domain.is_empty() {
            None
        } else {
            Some(domain)
        },
    )?;
    let password = sspi::Secret::new(rpassword::prompt_password("RDP password (hidden): ")?);
    if password.as_ref().len() > 65536 || password.as_ref().contains('\0') {
        return Err("invalid or oversized password".into());
    }
    Ok(AuthIdentity { username, password })
}

fn exchange(
    connection: &mut rustls::ClientConnection,
    stream: &mut TcpStream,
    host: &str,
    protocol: SecurityProtocol,
    user: Option<&str>,
    identity: Option<&AuthIdentity>,
) -> Result<(), Error> {
    let public_key = tls::subject_public_key(connection)?;
    let (pending, token) = ntlm::PendingNtlm::begin(identity, host)?;
    let mut request = TsRequest::new();
    request.nego_tokens.push(&token);
    tls::write_plaintext(connection, stream, &request.encode()?)?;
    let bytes = tls::read_message(connection, stream)?;
    let response = TsRequest::decode(&bytes)?;
    let version = response.check_server_status()?;
    if response.nego_tokens.len() != 1
        || response.auth_info.is_some()
        || response.pub_key_auth.is_some()
        || response.client_nonce.is_some()
    {
        return Err("unexpected CredSSP fields during NTLM challenge".into());
    }
    let challenge = response.nego_tokens[0];
    ntlm::validate_challenge(challenge)?;
    if identity.is_none() {
        println!(
            "CredSSP version {version}: received NTLM challenge with required signing/sealing capabilities."
        );
        println!("NLA probe only: no username, password or authentication response sent.");
        return Ok(());
    }
    let (protection, final_token) = pending.finish(challenge)?;
    let mut binding = BindingClient::new(protection, &public_key, version)?;
    tls::write_plaintext(connection, stream, &binding.request(&final_token)?)?;
    binding.verify(&tls::read_message(connection, stream)?)?;
    let (domain, user) = account(user.ok_or("missing account")?)?;
    let credentials = credentials::encode(
        domain,
        user,
        identity.ok_or("missing credentials")?.password.as_ref(),
    )?;
    tls::write_plaintext(connection, stream, &binding.delegate(&credentials)?)?;
    drop(credentials);
    if protocol == SecurityProtocol::CredSspEarlyAuth {
        tls::read_authorization(connection, stream)?;
        println!("CredSSP binding verified; Server early authorization succeeded.");
    } else {
        println!(
            "CredSSP binding verified and credentials delegated. Server has no early authorization result; desktop login is not confirmed."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntlm::tests::{TestServer, identity};
    use linrdp_proto::binding::Protection;
    use rustls::pki_types::{PrivatePkcs8KeyDer, ServerName};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
        time::{Duration, Instant},
    };

    // Real TLS and real NTLM crypto around a synthetic CredSSP peer. No OS login.
    fn round_trip(case: u8) -> Result<(), Error> {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = cert.signing_key.public_key_raw().to_vec();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let client_config = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let server_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
                )
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let peer = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut transport = rustls::StreamOwned::new(
                rustls::ServerConnection::new(server_config).unwrap(),
                stream,
            );
            let mut ntlm = TestServer::new(&identity("fixture password"));
            let bytes = tls::read_frame(&mut transport).unwrap();
            let first = TsRequest::decode(&bytes).unwrap();
            assert!(first.auth_info.is_none());
            let challenge = ntlm.accept(first.nego_tokens[0]).unwrap();
            let mut response = TsRequest::new();
            response.nego_tokens.push(&challenge);
            transport.write_all(&response.encode().unwrap()).unwrap();
            transport.flush().unwrap();
            if case == 4 {
                let mut next = [0];
                assert!(matches!(transport.read(&mut next), Ok(0) | Err(_)));
                return;
            }
            let bytes = tls::read_frame(&mut transport).unwrap();
            let binding = TsRequest::decode(&bytes).unwrap();
            assert!(binding.auth_info.is_none());
            if case == 1 {
                assert!(ntlm.accept(binding.nego_tokens[0]).is_err());
                let mut error = TsRequest::new();
                error.error_code = Some(0xc000006d);
                transport.write_all(&error.encode().unwrap()).unwrap();
                transport.flush().unwrap();
                return;
            }
            ntlm.accept(binding.nego_tokens[0]).unwrap();
            let mut protection = ntlm.protection();
            let hash = |label: &[u8]| {
                let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
                digest.update(label);
                digest.update(binding.client_nonce.unwrap());
                digest.update(&key);
                digest.finish().as_ref().to_vec()
            };
            assert_eq!(
                protection.unseal(binding.pub_key_auth.unwrap()).unwrap(),
                hash(b"CredSSP Client-To-Server Binding Hash\0")
            );
            let mut server_hash = hash(b"CredSSP Server-To-Client Binding Hash\0");
            if case == 2 {
                server_hash[0] ^= 1;
            }
            let sealed = protection.seal(&server_hash).unwrap();
            let mut response = TsRequest::new();
            response.pub_key_auth = Some(&sealed);
            transport.write_all(&response.encode().unwrap()).unwrap();
            transport.flush().unwrap();
            if case == 2 {
                let mut next = [0];
                assert!(matches!(transport.read(&mut next), Ok(0) | Err(_)));
                return;
            }
            let bytes = tls::read_frame(&mut transport).unwrap();
            let delegation = TsRequest::decode(&bytes).unwrap();
            assert!(delegation.nego_tokens.is_empty());
            assert!(delegation.pub_key_auth.is_none());
            let plaintext =
                zeroize::Zeroizing::new(protection.unseal(delegation.auth_info.unwrap()).unwrap());
            assert_eq!(
                *plaintext,
                *credentials::encode("LAB", "tester", "fixture password").unwrap()
            );
            let status = if case == 3 { 5u32 } else { 0u32 };
            transport.write_all(&status.to_le_bytes()).unwrap();
            transport.flush().unwrap();
        });
        let mut connection = tls::handshake(
            &mut client,
            ServerName::try_from("localhost").unwrap(),
            client_config,
            Instant::now() + Duration::from_secs(3),
        )
        .unwrap();
        let identity = identity(if case == 1 {
            "wrong password"
        } else {
            "fixture password"
        });
        let result = exchange(
            &mut connection,
            &mut client,
            "localhost",
            SecurityProtocol::CredSspEarlyAuth,
            if case == 4 { None } else { Some("LAB\\tester") },
            if case == 4 { None } else { Some(&identity) },
        );
        drop(connection);
        drop(client);
        peer.join().unwrap();
        result
    }

    #[test]
    fn completes_tls_ntlm_binding_delegation_and_early_authorization() {
        round_trip(0).unwrap();
    }

    #[test]
    fn handles_wrong_password_without_delegating_credentials() {
        assert!(
            round_trip(1)
                .unwrap_err()
                .to_string()
                .contains("0xc000006d")
        );
    }

    #[test]
    fn rejects_wrong_binding_before_delegating_credentials() {
        assert!(
            round_trip(2)
                .unwrap_err()
                .to_string()
                .contains("binding does not match")
        );
    }

    #[test]
    fn reports_early_authorization_denial() {
        assert!(
            round_trip(3)
                .unwrap_err()
                .to_string()
                .contains("authorization denied")
        );
    }

    #[test]
    fn probe_stops_after_challenge_without_authentication() {
        round_trip(4).unwrap();
    }
}
