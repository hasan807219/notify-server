use {
    crate::{
        auth::{
            add_ttl,
            from_jwt,
            sign_jwt,
            verify_identity,
            AuthError,
            SharedClaims,
            SubscriptionRequestAuth,
            SubscriptionResponseAuth,
        },
        handlers::subscribe_topic::ProjectData,
        spec::{NOTIFY_SUBSCRIBE_RESPONSE_TAG, NOTIFY_SUBSCRIBE_RESPONSE_TTL},
        state::AppState,
        types::{ClientData, Envelope, EnvelopeType0, EnvelopeType1},
        websocket_service::{
            derive_key,
            handlers::decrypt_message,
            NotifyMessage,
            NotifyResponse,
            NotifySubscribe,
        },
        Result,
    },
    base64::Engine,
    chrono::Utc,
    mongodb::bson::doc,
    relay_rpc::domain::{ClientId, DecodedClientId},
    serde_json::{json, Value},
    std::{sync::Arc, time::Duration},
    tracing::info,
    x25519_dalek::{PublicKey, StaticSecret},
};

// TODO idempotency
pub async fn handle(
    msg: relay_client::websocket::PublishedMessage,
    state: &Arc<AppState>,
    client: &Arc<relay_client::websocket::Client>,
) -> Result<()> {
    let request_id = uuid::Uuid::new_v4();
    let topic = msg.topic.to_string();

    // Grab record from db
    let project_data = state
        .database
        .collection::<ProjectData>("project_data")
        .find_one(doc!("topic": topic.clone()), None)
        .await?
        .ok_or(crate::error::Error::NoProjectDataForTopic(topic))?;

    let envelope = Envelope::<EnvelopeType1>::from_bytes(
        base64::engine::general_purpose::STANDARD.decode(msg.message.to_string())?,
    )?;

    let client_pubkey = envelope.pubkey();
    info!("[{request_id}] User pubkey: {}", hex::encode(client_pubkey));

    let response_sym_key = derive_key(
        hex::encode(client_pubkey),
        project_data.signing_keypair.private_key,
    )?;

    let msg: NotifyMessage<NotifySubscribe> = decrypt_message(envelope, &response_sym_key)?;

    info!("[{request_id}] Register message: {:?}", &msg);

    let id = msg.id;

    let sub_auth = from_jwt::<SubscriptionRequestAuth>(&msg.params.subscription_auth)?;
    let sub_auth_hash = sha256::digest(msg.params.subscription_auth);

    if sub_auth.act != "notify_subscription" {
        return Err(AuthError::InvalidAct)?;
    }

    verify_identity(&sub_auth.shared_claims.iss, &sub_auth.ksu, &sub_auth.sub).await?;

    // TODO verify `sub_auth.aud` matches `project_data.identity_keypair`

    // TODO verify `sub_auth.app` matches `project_data.dapp_url`

    // TODO merge code with integration.rs#verify_jwt()
    //      - put desired `iss` value as an argument to make sure we verify it

    let secret = StaticSecret::random_from_rng(chacha20poly1305::aead::OsRng);
    let public = PublicKey::from(&secret);

    let decoded_client_id = DecodedClientId(
        hex::decode(project_data.identity_keypair.public_key.clone())?[0..32].try_into()?,
    );
    let identity = ClientId::from(decoded_client_id).to_string();

    let decoded_client_id_public = DecodedClientId(public.to_bytes()[0..32].try_into()?);
    let identity_public = ClientId::from(decoded_client_id_public).to_string();

    let now = Utc::now();
    let response_message = SubscriptionResponseAuth {
        shared_claims: SharedClaims {
            iat: now.timestamp() as u64,
            exp: add_ttl(now, NOTIFY_SUBSCRIBE_RESPONSE_TTL).timestamp() as u64,
            iss: format!("did:key:{identity}"),
        },
        ksu: sub_auth.ksu.clone(),
        aud: sub_auth.shared_claims.iss,
        act: "notify_subscription_response".to_string(),
        sub: format!("did:key:{identity_public}"),
        app: project_data.dapp_url.to_string(),
    };
    let response_auth = sign_jwt(response_message, &project_data.identity_keypair)?;

    let response = NotifyResponse::<Value> {
        id,
        jsonrpc: "2.0".into(),
        result: json!({ "responseAuth": response_auth }), // TODO use structure
    };
    info!(
        "[{request_id}] Response for user: {}",
        serde_json::to_string(&response)?
    );

    let notify_key = derive_key(hex::encode(client_pubkey), hex::encode(secret))?;
    info!("[{request_id}] Derived push_key: {}", &notify_key);

    let envelope = Envelope::<EnvelopeType0>::new(&response_sym_key, response)?;

    let base64_notification = base64::engine::general_purpose::STANDARD.encode(envelope.to_bytes());

    let key = hex::decode(response_sym_key)?;
    let response_topic = sha256::digest(&*key);
    info!("[{request_id}] Response_topic: {}", &response_topic);

    let client_data = ClientData {
        id: sub_auth.sub.strip_prefix("did:pkh:").unwrap().to_owned(), // TODO remove unwrap(),
        relay_url: state.config.relay_url.clone(),
        sym_key: notify_key.clone(),
        scope: sub_auth.scp.split(' ').map(|s| s.to_owned()).collect(),
        expiry: sub_auth.shared_claims.exp,
        sub_auth_hash,
        ksu: sub_auth.ksu,
    };

    let notify_topic = sha256::digest(&*hex::decode(&notify_key)?);

    // This noop message is making relay aware that this topics TTL should be
    // extended
    info!(
        "[{request_id}] Sending settle message on topic {}",
        &notify_topic
    );

    // Registers account and subscribes to topic
    info!(
        "[{request_id}] Registering account: {:?} with topic: {} at project: {}. Scope: {:?}. Msg \
         id: {:?}",
        &client_data.id, &notify_topic, &project_data.id, &client_data.scope, &msg.id,
    );
    state
        .register_client(
            &project_data.id,
            client_data,
            &url::Url::parse(&state.config.relay_url)?,
        )
        .await?;

    // Send noop to extend ttl of relay's mapping
    client
        .publish(
            notify_topic.clone().into(),
            "",
            4050,
            Duration::from_secs(300),
            false,
        )
        .await?;

    info!(
        "[{request_id}] Settle message sent on topic {}",
        &notify_topic
    );

    client
        .publish(
            response_topic.into(),
            base64_notification,
            NOTIFY_SUBSCRIBE_RESPONSE_TAG,
            NOTIFY_SUBSCRIBE_RESPONSE_TTL,
            false,
        )
        .await?;

    Ok(())
}
