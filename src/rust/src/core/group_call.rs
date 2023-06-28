//
// Copyright 2019-2021 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only
//

use std::{
    collections::{HashMap, HashSet},
    convert::TryInto,
    hash::{Hash, Hasher},
    iter::FromIterator,
    mem::size_of,
    net::SocketAddr,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use hkdf::Hkdf;
use num_enum::TryFromPrimitive;
use prost::Message;
use rand::{rngs::OsRng, Rng};
use sha2::{Digest, Sha256};
use x25519_dalek::{EphemeralSecret, PublicKey};

use crate::{common::CallId, core::util::uuid_to_string};
use crate::{
    common::{
        actor::{Actor, Stopper},
        units::DataRate,
        DataMode, Result,
    },
    core::{call_mutex::CallMutex, crypto as frame_crypto, signaling},
    error::RingRtcError,
    lite::{
        http, sfu,
        sfu::{
            DemuxId, GroupMember, MembershipProof, PeekInfo, PeekResult, PeekResultCallback, UserId,
        },
    },
    protobuf,
    webrtc::{
        self,
        media::{AudioTrack, VideoFrame, VideoFrameMetadata, VideoSink, VideoTrack},
        peer_connection::{AudioLevel, PeerConnection, ReceivedAudioLevel, SendRates},
        peer_connection_factory::{self as pcf, IceServer, PeerConnectionFactory},
        peer_connection_observer::{
            IceConnectionState, NetworkRoute, PeerConnectionObserver, PeerConnectionObserverTrait,
        },
        rtp,
        sdp_observer::{create_ssd_observer, SessionDescription, SrtpCryptoSuite, SrtpKey},
        stats_observer::{create_stats_observer, StatsObserver},
    },
};

// Each instance of a group_call::Client has an ID for logging and passing events
// around (such as callbacks to the Observer).  It's just very convenient to have.
pub type ClientId = u32;
// Group UUID
pub type GroupId = Vec<u8>;
pub type GroupIdRef<'a> = &'a [u8];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingId(i64);

impl RingId {
    pub fn from_era_id(era_id: &str) -> Self {
        // Happy path: 16 hex digits
        if era_id.len() == 16 {
            if let Ok(i) = u64::from_str_radix(era_id, 16) {
                // We reserve 0 as an invalid ring ID; treat it as the equally-unlikely -1.
                // This does make -1 twice as likely! Out of 2^64 - 1 possibilities.
                if i == 0 {
                    return Self(-1);
                }
                return Self(i as i64);
            }
        }
        // Sad path: arbitrary strings get a truncated hash as their ring ID.
        // We have no current plans to change era IDs from being 16 hex digits,
        // but nothing enforces this today, and we may want to change them in the future.
        let truncated_hash: [u8; 8] = Sha256::digest(era_id.as_bytes()).as_slice()[..8]
            .try_into()
            .unwrap();
        Self(i64::from_le_bytes(truncated_hash))
    }
}

impl From<i64> for RingId {
    fn from(raw_id: i64) -> Self {
        Self(raw_id)
    }
}

impl From<RingId> for i64 {
    fn from(id: RingId) -> Self {
        id.0
    }
}

impl std::fmt::Display for RingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingUpdate {
    /// The sender is trying to ring this user.
    Requested = 0,
    /// The sender tried to ring this user, but it's been too long.
    ExpiredRequest,
    /// Call was accepted elsewhere by a different device.
    AcceptedOnAnotherDevice,
    /// Call was declined elsewhere by a different device.
    DeclinedOnAnotherDevice,
    /// This device is currently on a different call.
    BusyLocally,
    /// A different device is currently on a different call.
    BusyOnAnotherDevice,
    /// The sender cancelled the ring request.
    CancelledByRinger,
}

/// Describes why a ring was cancelled.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive)]
pub enum RingCancelReason {
    /// The user explicitly clicked "Decline".
    DeclinedByUser = 0,
    /// The device is busy with another call.
    Busy,
}

/// Indicates whether a signaling message should be marked for immediate processing
/// even if the receiving app isn't running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalingMessageUrgency {
    Droppable,
    HandleImmediately,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SrtpKeys {
    client: SrtpKey,
    server: SrtpKey,
}

impl SrtpKeys {
    const SUITE: SrtpCryptoSuite = SrtpCryptoSuite::AeadAes128Gcm;
    const KEY_LEN: usize = Self::SUITE.key_size();
    const SALT_LEN: usize = Self::SUITE.salt_size();
    const MASTER_KEY_MATERIAL_LEN: usize =
        Self::KEY_LEN + Self::SALT_LEN + Self::KEY_LEN + Self::SALT_LEN;

    fn from_master_key_material(master_key_material: &[u8; Self::MASTER_KEY_MATERIAL_LEN]) -> Self {
        Self {
            client: SrtpKey {
                suite: Self::SUITE,
                key: master_key_material[..Self::KEY_LEN].to_vec(),
                salt: master_key_material[Self::KEY_LEN..][..Self::SALT_LEN].to_vec(),
            },
            server: SrtpKey {
                suite: SrtpCryptoSuite::AeadAes128Gcm,
                key: master_key_material[Self::KEY_LEN..][Self::SALT_LEN..][..Self::KEY_LEN]
                    .to_vec(),
                salt: master_key_material[Self::KEY_LEN..][Self::SALT_LEN..][Self::KEY_LEN..]
                    [..Self::SALT_LEN]
                    .to_vec(),
            },
        }
    }
}

pub const INVALID_CLIENT_ID: ClientId = 0;

#[derive(Debug)]
pub enum RemoteDevicesChangedReason {
    DemuxIdsChanged,
    MediaKeyReceived(DemuxId),
    SpeakerTimeChanged(DemuxId),
    HeartbeatStateChanged(DemuxId),
    ForwardedVideosChanged,
    HigherResolutionPendingChanged,
}

// The callbacks from the Call to the Observer of the call.
// Some of these are more than an "observer" in that a response is needed,
// which is provided asynchronously.
pub trait Observer {
    // A response should be provided via Call.update_membership_proof.
    fn request_membership_proof(&self, client_id: ClientId);
    // A response should be provided via Call.update_group_members.
    fn request_group_members(&self, client_id: ClientId);
    // Send a signaling message to the given remote user
    fn send_signaling_message(
        &mut self,
        recipient: UserId,
        message: protobuf::signaling::CallMessage,
        urgency: SignalingMessageUrgency,
    );
    // Send a signaling message to all members of the group.
    fn send_signaling_message_to_group(
        &mut self,
        group: GroupId,
        message: protobuf::signaling::CallMessage,
        urgency: SignalingMessageUrgency,
    );

    // The following notify the observer of state changes to the local device.
    fn handle_connection_state_changed(
        &self,
        client_id: ClientId,
        connection_state: ConnectionState,
    );
    fn handle_network_route_changed(&self, client_id: ClientId, network_route: NetworkRoute);
    fn handle_join_state_changed(&self, client_id: ClientId, join_state: JoinState);
    fn handle_send_rates_changed(&self, _client_id: ClientId, _send_rates: SendRates) {}

    // The following notify the observer of state changes to the remote devices.
    fn handle_remote_devices_changed(
        &self,
        client_id: ClientId,
        remote_devices: &[RemoteDeviceState],
        reason: RemoteDevicesChangedReason,
    );

    // Notifies the observer of changes to the list of call participants.
    fn handle_peek_changed(
        &self,
        client_id: ClientId,
        peek_info: &PeekInfo,
        // We use a HashSet because the client expects a unique list of users,
        // and there can be multiple devices from the same user.
        joined_members: &HashSet<UserId>,
    );

    // This is separate from handle_remote_devices_changed because everything else
    // is a pure state that can be copied, deleted, etc.
    // But the VideoTrack is a special handle which must be attached to.
    // This will be called once per demux_id after handle_remote_devices_changed
    // has been called with the demux_id included.
    fn handle_incoming_video_track(
        &mut self,
        client_id: ClientId,
        remote_demux_id: DemuxId,
        incoming_video_track: VideoTrack,
    );

    fn handle_audio_levels(
        &self,
        client_id: ClientId,
        captured_level: AudioLevel,
        received_levels: Vec<ReceivedAudioLevel>,
    );

    // This will be the last callback.
    // The observer can assume the Call is completely shut down and can be deleted.
    fn handle_ended(&self, client_id: ClientId, reason: EndReason);
}

// The connection states of a device connecting to a group call.
// Has a state diagram like this:
//
//      |
//      | start()
//      V
// NotConnected
//      |                        ^
//      | connect()              |
//      V                        |
//  Connecting                -->|
//      |                        |
//      | connected              | connection failed
//      V                        | or disconnect()
//  Connected                 -->|
//      |            ^           |
//      | problems   | fixed     |
//      V            |           |
// Reconnecting               -->|
//
// Currently, due to limitations of the SFU, we cannot connect until after join() is called.
// So the ConnectionState will remain Connecting until join() is called.
// But updates to members joined (via handle_peek_changed)
// will still be received even when only Connecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connect() has not yet been called
    /// or disconnect() has been called
    /// or connect() was called but failed.
    NotConnected,

    /// Connect() has been called but connectivity is pending.
    Connecting,

    /// Connect() has been called and connectivity has been established.
    Connected,

    /// Connect() has been called and connection has been established.
    /// But the connectivity is temporarily failing.
    Reconnecting,
}

// The join states of a device joining a group call.
// Has a state diagram like this:
//      |
//      | start()
//      V
//  NotJoined
//      |            ^
//      | join()     |
//      V            |
//   Joining      -->|  leave() or
//      |            |  failed to join
//      | joined     |
//      V            |
//   Joined       -->|
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinState {
    /// Join() has not yet been called
    /// or leave() has been called
    /// or join() was called but failed.
    ///
    /// If the ring ID is present,
    /// joining will sent an "accepted" message to your other devices.
    NotJoined(Option<RingId>),

    /// Join() has been called but a response from the SFU is pending.
    Joining,

    /// Join() has been called, a response from the SFU has been received,
    /// and a DemuxId has been assigned.
    Joined(DemuxId),
}

// This really should go in JoinState and/or ConnectionState,
// but an EphemeralSecret isn't Clone or Debug, so it's inconvenient
// to put them in there.  Plus, because of the weird relationship
// between the ConnectionState and JoinState due to limitations of
// the SFU (not being able to connect until after joined), it's
// also more convenient to call GroupCall::start_peer_connection
// with a state separate from those 2.
enum DheState {
    NotYetStarted,
    WaitingForServerPublicKey { client_secret: EphemeralSecret },
    Negotiated { srtp_keys: SrtpKeys },
}

impl Default for DheState {
    fn default() -> Self {
        Self::NotYetStarted
    }
}

impl DheState {
    fn start(client_secret: EphemeralSecret) -> Self {
        DheState::WaitingForServerPublicKey { client_secret }
    }

    fn negotiate_in_place(&mut self, server_pub_key: &PublicKey, hkdf_extra_info: &[u8]) {
        *self = std::mem::take(self).negotiate(server_pub_key, hkdf_extra_info)
    }

    fn negotiate(self, server_pub_key: &PublicKey, hkdf_extra_info: &[u8]) -> Self {
        match self {
            DheState::NotYetStarted => {
                error!("Attempting to negotiated SRTP keys before starting DHE.");
                self
            }
            DheState::WaitingForServerPublicKey { client_secret } => {
                let shared_secret = client_secret.diffie_hellman(server_pub_key);
                let mut master_key_material = [0u8; SrtpKeys::MASTER_KEY_MATERIAL_LEN];
                Hkdf::<Sha256>::new(Some(&[0u8; 32]), shared_secret.as_bytes())
                    .expand_multi_info(
                        &[
                            b"Signal_Group_Call_20211105_SignallingDH_SRTPKey_KDF",
                            hkdf_extra_info,
                        ],
                        &mut master_key_material,
                    )
                    .expect("SRTP master key material expansion");
                DheState::Negotiated {
                    srtp_keys: SrtpKeys::from_master_key_material(&master_key_material),
                }
            }
            DheState::Negotiated { .. } => {
                warn!("Attempting to negotiated SRTP keys a second time.");
                self
            }
        }
    }
}

// The info about SFU needed in order to connect to it.
#[derive(Clone, Debug)]
pub struct SfuInfo {
    pub udp_addresses: Vec<SocketAddr>,
    pub tcp_addresses: Vec<SocketAddr>,
    pub ice_ufrag: String,
    pub ice_pwd: String,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EndReason {
    // Normal events
    DeviceExplicitlyDisconnected = 0,
    ServerExplicitlyDisconnected,
    DeniedRequestToJoinCall,
    RemovedFromCall,

    // Things that can go wrong
    CallManagerIsBusy,
    SfuClientFailedToJoin,
    FailedToCreatePeerConnectionFactory,
    FailedToNegotiatedSrtpKeys,
    FailedToCreatePeerConnection,
    FailedToStartPeerConnection,
    FailedToUpdatePeerConnection,
    FailedToSetMaxSendBitrate,
    IceFailedWhileConnecting,
    IceFailedAfterConnected,
    ServerChangedDemuxId,
    HasMaxDevices,
}

// The callbacks from the Client to the "SFU client" for the group call.
pub trait SfuClient {
    // This should call Client.on_sfu_client_joined when the SfuClient has joined.
    fn join(&mut self, ice_ufrag: &str, dhe_pub_key: [u8; 32], client: Client);
    fn peek(&mut self, result_callback: PeekResultCallback);

    // Notifies the client of the new membership proof.
    fn set_membership_proof(&mut self, proof: MembershipProof);
    fn set_group_members(&mut self, members: Vec<GroupMember>);
}

pub struct Joined {
    pub sfu_info: SfuInfo,
    pub local_demux_id: DemuxId,
    pub server_dhe_pub_key: [u8; 32],
    pub hkdf_extra_info: Vec<u8>,
    pub creator: Option<UserId>,
    pub era_id: String,
}

/// Communicates with the SFU using HTTP.
pub struct HttpSfuClient {
    sfu_url: String,
    room_id_header: Option<String>,
    admin_passkey: Option<Vec<u8>>,
    // For use post-DHE
    hkdf_extra_info: Vec<u8>,
    http_client: Box<dyn http::Client + Send>,
    auth_header: Option<String>,
    member_resolver: Arc<dyn sfu::MemberResolver + Send + Sync>,
    deferred_join: Option<(String, [u8; 32], Client)>,
}

impl HttpSfuClient {
    pub fn new(
        http_client: Box<dyn http::Client + Send>,
        url: String,
        room_id_for_header: Option<&[u8]>,
        admin_passkey: Option<Vec<u8>>,
        hkdf_extra_info: Vec<u8>,
    ) -> Self {
        Self {
            sfu_url: url,
            room_id_header: room_id_for_header.map(hex::encode),
            admin_passkey,
            hkdf_extra_info,
            http_client,
            auth_header: None,
            member_resolver: Arc::new(sfu::MemberMap::default()),
            deferred_join: None,
        }
    }

    pub fn set_auth_header(&mut self, auth_header: String) {
        self.auth_header = Some(auth_header)
    }

    pub fn set_member_resolver(
        &mut self,
        member_resolver: Arc<dyn sfu::MemberResolver + Send + Sync>,
    ) {
        self.member_resolver = member_resolver;
    }

    fn join_with_header(
        &self,
        auth_header: String,
        ice_ufrag: &str,
        dhe_pub_key: &[u8],
        client: Client,
    ) {
        let hkdf_extra_info = self.hkdf_extra_info.clone();
        sfu::join(
            self.http_client.as_ref(),
            &self.sfu_url,
            self.room_id_header.clone(),
            auth_header,
            self.admin_passkey.as_deref(),
            ice_ufrag,
            dhe_pub_key,
            &self.hkdf_extra_info,
            self.member_resolver.clone(),
            Box::new(move |join_response| {
                let join_result: Result<Joined> = match join_response {
                    Ok(join_response) => Ok(Joined {
                        sfu_info: SfuInfo {
                            udp_addresses: join_response.server_udp_addresses,
                            tcp_addresses: join_response.server_tcp_addresses,
                            ice_ufrag: join_response.server_ice_ufrag,
                            ice_pwd: join_response.server_ice_pwd,
                        },
                        local_demux_id: join_response.client_demux_id,
                        server_dhe_pub_key: join_response.server_dhe_pub_key,
                        creator: join_response.call_creator,
                        era_id: join_response.era_id,
                        hkdf_extra_info,
                    }),
                    Err(http_status) if http_status == http::ResponseStatus::REQUEST_FAILED => {
                        Err(RingRtcError::SfuClientRequestFailed.into())
                    }
                    Err(http_status) if http_status == http::ResponseStatus::GROUP_CALL_FULL => {
                        Err(RingRtcError::GroupCallFull.into())
                    }
                    Err(http_status) => {
                        Err(RingRtcError::UnexpectedResponseCodeFromSFu(http_status.code).into())
                    }
                };
                client.on_sfu_client_joined(join_result);
            }),
        );
    }
}

impl SfuClient for HttpSfuClient {
    fn set_membership_proof(&mut self, proof: MembershipProof) {
        if let Some(auth_header) = sfu::auth_header_from_membership_proof(&proof) {
            self.auth_header = Some(auth_header.clone());
            // Release any tasks that were blocked on getting the token.
            if let Some((ice_ufrag, dhe_pub_key, client)) = self.deferred_join.take() {
                info!("membership token received, proceeding with deferred join");
                self.join_with_header(auth_header, &ice_ufrag, &dhe_pub_key[..], client);
            }
        }
    }

    fn join(&mut self, ice_ufrag: &str, dhe_pub_key: [u8; 32], client: Client) {
        match self.auth_header.as_ref() {
            Some(h) => self.join_with_header(h.clone(), ice_ufrag, &dhe_pub_key[..], client),
            None => {
                info!("join requested without membership token - deferring");
                let ice_ufrag = ice_ufrag.to_string();
                self.deferred_join = Some((ice_ufrag, dhe_pub_key, client));
            }
        }
    }

    fn peek(&mut self, result_callback: PeekResultCallback) {
        match self.auth_header.clone() {
            Some(auth_header) => sfu::peek(
                self.http_client.as_ref(),
                &self.sfu_url,
                self.room_id_header.clone(),
                auth_header,
                self.member_resolver.clone(),
                result_callback,
            ),
            None => {
                result_callback(Err(http::ResponseStatus::INVALID_CLIENT_AUTH));
            }
        }
    }

    fn set_group_members(&mut self, members: Vec<GroupMember>) {
        info!("SfuClient set_group_members: {} members", members.len());
        self.set_member_resolver(Arc::new(sfu::MemberMap::new(&members)));
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct HeartbeatState {
    pub audio_muted: Option<bool>,
    pub video_muted: Option<bool>,
    pub presenting: Option<bool>,
    pub sharing_screen: Option<bool>,
}

impl From<protobuf::group_call::device_to_device::Heartbeat> for HeartbeatState {
    fn from(proto: protobuf::group_call::device_to_device::Heartbeat) -> Self {
        Self {
            audio_muted: proto.audio_muted,
            video_muted: proto.video_muted,
            presenting: proto.presenting,
            sharing_screen: proto.sharing_screen,
        }
    }
}

// The info about remote devices received from the SFU
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDeviceState {
    pub demux_id: DemuxId,
    pub user_id: UserId,
    pub media_keys_received: bool,
    pub heartbeat_state: HeartbeatState,
    // The latest timestamp we received from an update to
    // heartbeat_state.
    heartbeat_rtp_timestamp: Option<rtp::Timestamp>,
    // The time at which this device was added to the list of devices.
    // A combination of (added_timestamp, demux_id) can be used for a stable
    // sort of remote devices for a grid layout.
    pub added_time: SystemTime,
    // The most recent time at which this device became the primary speaker
    // Sorting using this value will give a history of who spoke.
    pub speaker_time: Option<SystemTime>,
    pub leaving_received: bool,
    pub forwarding_video: Option<bool>,
    pub server_allocated_height: u16,
    pub client_decoded_height: Option<u32>,
    pub is_higher_resolution_pending: bool,
}

fn as_unix_millis(t: Option<SystemTime>) -> u64 {
    if let Some(t) = t {
        if let Ok(d) = t.duration_since(SystemTime::UNIX_EPOCH) {
            d.as_millis() as u64
        } else {
            0
        }
    } else {
        0
    }
}

impl RemoteDeviceState {
    fn new(demux_id: DemuxId, user_id: UserId, added_time: SystemTime) -> Self {
        Self {
            demux_id,
            user_id,
            media_keys_received: false,
            heartbeat_state: Default::default(),
            heartbeat_rtp_timestamp: None,

            added_time,
            speaker_time: None,
            leaving_received: false,
            forwarding_video: None,
            server_allocated_height: 0,
            client_decoded_height: None,
            is_higher_resolution_pending: false,
        }
    }

    pub fn speaker_time_as_unix_millis(&self) -> u64 {
        as_unix_millis(self.speaker_time)
    }

    pub fn added_time_as_unix_millis(&self) -> u64 {
        as_unix_millis(Some(self.added_time))
    }

    fn recalculate_higher_resolution_pending(&mut self) {
        let was_pending = self.is_higher_resolution_pending;
        self.is_higher_resolution_pending =
            self.server_allocated_height as u32 > self.client_decoded_height.unwrap_or(0);

        if !was_pending && self.is_higher_resolution_pending {
            info!(
                "Higher resolution video (height={}) now pending for {}. Current height is {:?}",
                self.server_allocated_height, self.demux_id, self.client_decoded_height
            );
        }
    }
}

/// These can be sent to the SFU to request different resolutions of
/// video for different remote dem
#[derive(Clone, Debug)]
pub struct VideoRequest {
    pub demux_id: DemuxId,
    pub width: u16,
    pub height: u16,
    // If not specified, it means unrestrained framerate.
    pub framerate: Option<u16>,
}

// This must stay in sync with the data PT in SfuClient.
const RTP_DATA_PAYLOAD_TYPE: rtp::PayloadType = 101;
// This must stay in sync with the data SSRC offset in SfuClient.
const RTP_DATA_THROUGH_SFU_SSRC_OFFSET: rtp::Ssrc = 0xD;
const RTP_DATA_TO_SFU_SSRC: rtp::Ssrc = 1;

// If the local device is the only device, tell WebRTC to send as little
// as possible while keeping the bandwidth estimator going.
// It looks like the bandwidth estimator will only probe up to 100kbps,
// but that's better than nothing.  It appears to take 26 seconds to
// ramp all the way up, though.
const ALL_ALONE_MAX_SEND_RATE: DataRate = DataRate::from_kbps(1);

const SMALL_CALL_MAX_SEND_RATE: DataRate = DataRate::from_kbps(1000);

// This is the smallest rate at which WebRTC seems to still send VGA.
const LARGE_CALL_MAX_SEND_RATE: DataRate = DataRate::from_kbps(671);

// Use a higher bitrate for screen sharing
const SCREENSHARE_MIN_SEND_RATE: DataRate = DataRate::from_mbps(2);
const SCREENSHARE_START_SEND_RATE: DataRate = DataRate::from_mbps(2);
const SCREENSHARE_MAX_SEND_RATE: DataRate = DataRate::from_mbps(5);

const LOW_MAX_RECEIVE_RATE: DataRate = DataRate::from_kbps(500);

const NORMAL_MAX_RECEIVE_RATE: DataRate = DataRate::from_mbps(20);

// The time between when a sender generates a new media send key
// and applies it.  It needs to be big enough that there is
// a high probability that receivers will receive the
// key before the sender begins using it.  But making it too big
// gives a larger window of time during which a receiver that has
// left the call may decrypt media after leaving.
// Note that the window can be almost double this value because
// only one media send key rotation can be pending at a time
// so a receiver may leave immediately after receiving a newly
// generated key and it will be able to decrypt until after
// a second rotation is applied.
const MEDIA_SEND_KEY_ROTATION_DELAY_SECS: u64 = 3;

enum KeyRotationState {
    // A key has been applied.  Nothing is pending.
    Applied,
    // A key has been generated but not yet applied.
    Pending {
        secret: frame_crypto::Secret,
        // Once it has been applied, another rotation needs to take place because
        // a user left the call while rotation was pending.
        needs_another_rotation: bool,
    },
}

// We want to make sure there is at most one pending request for remote devices
// going on at a time, and to only request remote devices when the data is too stale
// or if it's been too long without a response.
#[derive(Debug)]
enum RemoteDevicesRequestState {
    WaitingForMembershipProof,
    NeverRequested,
    Requested {
        // While waiting, something happened that makes us think we should ask again.
        should_request_again: bool,
        at: Instant,
    },
    Updated {
        at: Instant,
    },
    Failed {
        at: Instant,
    },
}

/// Represents a device connecting to an SFU and joining a group call.
#[derive(Clone)]
pub struct Client {
    // A value used for logging and passing into the Observer.
    client_id: ClientId,
    pub group_id: GroupId,
    // We have to leave this outside of the actor state
    // because WebRTC calls back to the PeerConnectionObserver
    // synchronously.
    frame_crypto_context: Arc<CallMutex<frame_crypto::Context>>,
    actor: Actor<State>,
}

#[derive(Default)]
struct RemoteDevices(Vec<RemoteDeviceState>);

impl Deref for RemoteDevices {
    type Target = Vec<RemoteDeviceState>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RemoteDevices {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FromIterator<RemoteDeviceState> for RemoteDevices {
    fn from_iter<T: IntoIterator<Item = RemoteDeviceState>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for RemoteDevices {
    type Item = RemoteDeviceState;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[derive(Debug)]
enum OutgoingRingState {
    /// The initial state
    Unknown,
    /// The local client is permitted to send a ring if they choose, but has not requested one.
    PermittedToRing { ring_id: RingId },
    /// The local client has requested to ring, but it is unknown whether it is permitted.
    WantsToRing { recipient: Option<UserId> },
    /// The local client has, in fact, sent a ring (and may still cancel it).
    HasSentRing { ring_id: RingId },
    /// The local client is not permitted to send rings at this time.
    ///
    /// They may not be the creator of the call, or they may have already sent a ring and had other
    /// people join.
    NotPermittedToRing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupCallKind {
    SignalGroup,
    CallLink,
}

/// The state inside the Actor
struct State {
    // Things passed in that never change
    client_id: ClientId,
    group_id: GroupId,
    kind: GroupCallKind,
    sfu_client: Box<dyn SfuClient>,
    observer: Box<dyn Observer>,

    // Shared state with the CallManager that might change
    busy: Arc<CallMutex<bool>>,
    self_uuid: Arc<CallMutex<Option<UserId>>>,

    // State that changes regularly and is sent to the observer
    connection_state: ConnectionState,
    join_state: JoinState,
    remote_devices: RemoteDevices,
    has_ever_been_participating_client: bool,

    // State that changes infrequently and is not sent to the observer.
    dhe_state: DheState,

    // Things to control peeking
    remote_devices_request_state: RemoteDevicesRequestState,
    last_peek_info: Option<PeekInfo>,
    known_members: HashSet<UserId>,

    // Derived from remote_devices but stored so we can fire
    // Observer::handle_peek_changed only when it changes
    joined_members: HashSet<UserId>,
    pending_users_signature: u64,

    // Things we send to other clients via heartbeats
    // These are unset until the app sets them.
    // But we err on the side of caution and don't send anything when they are unset.
    outgoing_heartbeat_state: HeartbeatState,

    // Things for controlling the PeerConnection
    local_ice_ufrag: String,
    local_ice_pwd: String,
    sfu_info: Option<SfuInfo>,
    peer_connection: PeerConnection,
    peer_connection_observer_impl: Box<PeerConnectionObserverImpl>,
    rtp_data_to_sfu_next_seqnum: u32,
    rtp_data_through_sfu_next_seqnum: u32,
    next_heartbeat_time: Option<Instant>,

    // Things for getting statistics from the PeerConnection
    // Stats gathering happens only when joined
    next_stats_time: Option<Instant>,
    stats_observer: Box<StatsObserver>,

    audio_levels_interval: Option<Duration>,
    // Things for getting audio levels from the PeerConnection
    next_audio_levels_time: Option<Instant>,

    next_membership_proof_request_time: Option<Instant>,

    // We have to put this inside the actor state also because
    // we change the keys from within the actor.
    frame_crypto_context: Arc<CallMutex<frame_crypto::Context>>,

    // If we receive a media key before we know about the remote device,
    // we store it here until we do know about the remote device.
    pending_media_receive_keys: Vec<(
        UserId,
        DemuxId,
        frame_crypto::RatchetCounter,
        frame_crypto::Secret,
    )>,
    // If we generate a new media send key when a user leaves the call,
    // during the time between when we generate it and apply it, we need
    // to make sure that user that joined in that window gets that key
    // even if it hasn't been applied yet.
    // And if more than one user leaves at the same time, we want to make sure
    // we throttle the rotations so they don't happen too often.
    // Note that this has the effect of doubling the amount of time someone might
    // be able do decrypt media after leaving if they leave immediately
    // after receiving a newly generated key.
    media_send_key_rotation_state: KeyRotationState,

    // Things to control video requests.  We want to send them regularly on ticks,
    // but also limit how often they are sent "on demand".  So here's the rule:
    // once per second, you get an "on demand" one.  Any more than that and you
    // wait for the next tick.
    video_requests: Option<Vec<VideoRequest>>,
    active_speaker_height: Option<u16>,
    on_demand_video_request_sent_since_last_heartbeat: bool,
    speaker_rtp_timestamp: Option<rtp::Timestamp>,

    send_rates: SendRates,
    // If set, will always overide the send_rates.  Intended for testing.
    send_rates_override: Option<SendRates>,
    max_receive_rate: Option<DataRate>,
    data_mode: DataMode,
    // Demux IDs where video is being forward from, mapped to the server allocated height.
    forwarding_videos: HashMap<DemuxId, u16>,

    outgoing_ring_state: OutgoingRingState,

    actor: Actor<State>,
}

impl RemoteDevices {
    /// Find the latest speaker
    fn latest_speaker_demux_id(&self) -> Option<DemuxId> {
        let latest_speaker = self.iter().max_by_key(|a| a.speaker_time);
        if latest_speaker?.speaker_time.is_none() {
            None
        } else {
            latest_speaker.map(|speaker| speaker.demux_id)
        }
    }

    /// Find remote device state by demux id
    fn find_by_demux_id(&self, demux_id: DemuxId) -> Option<&RemoteDeviceState> {
        self.iter().find(|device| device.demux_id == demux_id)
    }

    /// Find remote device state by demux id
    fn find_by_demux_id_mut(&mut self, demux_id: DemuxId) -> Option<&mut RemoteDeviceState> {
        self.0.iter_mut().find(|device| device.demux_id == demux_id)
    }

    /// Returns a set containing all the demux ids in the collection
    fn demux_id_set(&self) -> HashSet<DemuxId> {
        self.iter().map(|device| device.demux_id).collect()
    }
}

// The time between ticks to do periodic things like request updated
// membership list from the SfuClient
const TICK_INTERVAL: Duration = Duration::from_millis(200);

// How often to send RTP data messages and video requests.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

// How often to get and log stats.
const STATS_INTERVAL: Duration = Duration::from_secs(10);
const STATS_INITIAL_OFFSET: Duration = Duration::from_secs(2);

// How often to request an updated membership proof (24 hours).
const MEMBERSHIP_PROOF_REQUEST_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

impl Client {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        group_id: GroupId,
        client_id: ClientId,
        kind: GroupCallKind,
        sfu_client: Box<dyn SfuClient + Send>,
        observer: Box<dyn Observer + Send>,
        busy: Arc<CallMutex<bool>>,
        self_uuid: Arc<CallMutex<Option<UserId>>>,
        peer_connection_factory: Option<PeerConnectionFactory>,
        outgoing_audio_track: AudioTrack,
        outgoing_video_track: Option<VideoTrack>,
        // This is separate from the observer so it can bypass a thread hop.
        incoming_video_sink: Option<Box<dyn VideoSink>>,
        ring_id: Option<RingId>,
        audio_levels_interval: Option<Duration>,
    ) -> Result<Self> {
        debug!("group_call::Client(outer)::new(client_id: {})", client_id);
        let stopper = Stopper::new();
        // We only send with this key until the first person joins, at which point
        // we ratchet the key forward.
        let frame_crypto_context = Arc::new(CallMutex::new(
            frame_crypto::Context::new(frame_crypto::random_secret(&mut rand::rngs::OsRng)),
            "Frame encryption context",
        ));
        let frame_crypto_context_for_outside_actor = frame_crypto_context.clone();
        let client = Self {
            client_id,
            group_id: group_id.clone(),
            actor: Actor::start(stopper, move |actor| {
                debug!("group_call::Client(inner)::new(client_id: {})", client_id);

                let peer_connection_factory = match peer_connection_factory {
                    None => match PeerConnectionFactory::new(pcf::Config::default()) {
                        Ok(v) => v,
                        Err(err) => {
                            observer.handle_ended(
                                client_id,
                                EndReason::FailedToCreatePeerConnectionFactory,
                            );
                            return Err(err);
                        }
                    },
                    Some(v) => v,
                };

                let (peer_connection_observer_impl, peer_connection_observer) =
                    PeerConnectionObserverImpl::uninitialized(incoming_video_sink)?;
                // WebRTC uses alphanumeric plus + and /, which is just barely a superset of this,
                // but we can't uses dashes due to the sfu.
                let local_ice_ufrag = random_alphanumeric(4);
                let local_ice_pwd = random_alphanumeric(22);
                let audio_jitter_buffer_max_packets = 50;
                let ice_server = IceServer::none();
                let peer_connection = peer_connection_factory
                    .create_peer_connection(
                        peer_connection_observer,
                        pcf::RffiPeerConnectionKind::GroupCall,
                        audio_jitter_buffer_max_packets,
                        &ice_server,
                        outgoing_audio_track,
                        outgoing_video_track,
                    )
                    .map_err(|e| {
                        observer.handle_ended(client_id, EndReason::FailedToCreatePeerConnection);
                        e
                    })?;
                let call_id_for_stats = CallId::from(client_id as u64);
                info!(
                    "ringrtc_stats!,\
                        sfu,\
                        recv,\
                        target_send_rate,\
                        ideal_send_rate,\
                        allocated_send_rate"
                );
                Ok(State {
                    client_id,
                    group_id,
                    kind,
                    sfu_client,
                    observer,
                    busy,
                    self_uuid,
                    local_ice_ufrag,
                    local_ice_pwd,

                    connection_state: ConnectionState::NotConnected,
                    join_state: JoinState::NotJoined(ring_id),
                    dhe_state: DheState::default(),
                    remote_devices: Default::default(),
                    has_ever_been_participating_client: false,

                    remote_devices_request_state: match kind {
                        GroupCallKind::SignalGroup => {
                            RemoteDevicesRequestState::WaitingForMembershipProof
                        }
                        GroupCallKind::CallLink => RemoteDevicesRequestState::NeverRequested,
                    },
                    last_peek_info: None,

                    known_members: HashSet::new(),

                    joined_members: HashSet::new(),
                    pending_users_signature: 0,

                    outgoing_heartbeat_state: Default::default(),

                    sfu_info: None,
                    peer_connection_observer_impl,
                    peer_connection,
                    rtp_data_to_sfu_next_seqnum: 1,
                    rtp_data_through_sfu_next_seqnum: 1,

                    next_heartbeat_time: None,

                    next_stats_time: None,
                    stats_observer: create_stats_observer(call_id_for_stats, STATS_INTERVAL),

                    audio_levels_interval,
                    next_audio_levels_time: None,

                    next_membership_proof_request_time: None,

                    frame_crypto_context,
                    pending_media_receive_keys: Vec::new(),
                    media_send_key_rotation_state: KeyRotationState::Applied,

                    video_requests: None,
                    active_speaker_height: None,
                    on_demand_video_request_sent_since_last_heartbeat: false,
                    speaker_rtp_timestamp: None,

                    send_rates: SendRates::default(),
                    send_rates_override: None,
                    // If the client never calls set_data_mode, use the normal max receive rate.
                    max_receive_rate: Some(NORMAL_MAX_RECEIVE_RATE),
                    data_mode: DataMode::Normal,
                    forwarding_videos: HashMap::default(),

                    outgoing_ring_state: OutgoingRingState::Unknown,

                    actor,
                })
            })?,
            frame_crypto_context: frame_crypto_context_for_outside_actor,
        };

        // After we have the actor, we can initialize the PeerConnectionObserverImpl
        // and kick of ticking.
        let client_clone_to_init_peer_connection_observer_impl = client.clone();
        client.actor.send(move |state| {
            state
                .peer_connection_observer_impl
                .initialize(client_clone_to_init_peer_connection_observer_impl);
            Self::request_remote_devices_as_soon_as_possible(state);
        });
        Ok(client)
    }

    pub fn provide_ring_id_if_absent(&self, ring_id: RingId) {
        self.actor.send(move |state| match &mut state.join_state {
            JoinState::NotJoined(Some(existing_ring_id)) => {
                // Note that we prefer older rings to newer, unlike when processing incoming rings.
                // This is because we expect the call to already be handling the existing ring
                // (maybe that's what's actively ringing in the app).
                warn!(
                    "discarding ring {}; already have a ring for the same group ({})",
                    ring_id, existing_ring_id
                );
            }
            JoinState::NotJoined(saved_ring_id) => {
                debug_assert!(saved_ring_id.is_none());
                *saved_ring_id = Some(ring_id);
            }
            JoinState::Joining | JoinState::Joined(_) => {
                warn!(
                    "ignoring ring {} for a call we have already joined or are currently joining",
                    ring_id
                );
            }
        });
    }

    // Should only be used for testing
    pub fn override_send_rates(&self, send_rates_override: SendRates) {
        self.actor.send(move |state| {
            state.send_rates_override = Some(send_rates_override.clone());
            Self::set_send_rates_inner(state, send_rates_override);
        });
    }

    // Pulled into a named private method so we can call it recursively.
    fn tick(state: &mut State) {
        let now = Instant::now();

        trace!(
            "group_call::Client(inner)::tick(group_id: {})",
            state.client_id
        );

        Self::request_remote_devices_from_sfu_if_older_than(state, Duration::from_secs(10));

        if let Some(next_heartbeat_time) = state.next_heartbeat_time {
            if now >= next_heartbeat_time {
                if let Err(err) = Self::send_heartbeat(state) {
                    warn!("Failed to send regular heartbeat: {:?}", err);
                }
                // Also send video requests at the same rate as the hearbeat.
                Self::send_video_requests_to_sfu(state);
                state.on_demand_video_request_sent_since_last_heartbeat = false;
                state.next_heartbeat_time = Some(now + HEARTBEAT_INTERVAL)
            }
        }

        if let Some(next_stats_time) = state.next_stats_time {
            if now >= next_stats_time {
                let _ = state
                    .peer_connection
                    .get_stats(state.stats_observer.as_ref());
                state.next_stats_time = Some(now + STATS_INTERVAL);
            }
        }

        if let (Some(audio_levels_interval), Some(next_audio_levels_time)) =
            (state.audio_levels_interval, state.next_audio_levels_time)
        {
            if now >= next_audio_levels_time {
                let (captured_level, received_levels) = state.peer_connection.get_audio_levels();
                state.observer.handle_audio_levels(
                    state.client_id,
                    captured_level,
                    received_levels,
                );
                state.next_audio_levels_time = Some(now + audio_levels_interval);
            }
        }

        if state.kind == GroupCallKind::SignalGroup {
            if let Some(next_membership_proof_request_time) =
                state.next_membership_proof_request_time
            {
                if now >= next_membership_proof_request_time {
                    state.observer.request_membership_proof(state.client_id);
                    state.next_membership_proof_request_time =
                        Some(now + MEMBERSHIP_PROOF_REQUEST_INTERVAL);
                }
            }
        }

        state.actor.send_delayed(TICK_INTERVAL, Self::tick);
    }

    fn request_remote_devices_as_soon_as_possible(state: &mut State) {
        debug!(
            "group_call::Client::request_remote_devices_as_soon_as_possible(client_id: {})",
            state.client_id
        );

        Self::maybe_request_remote_devices(state, Duration::from_secs(0), true);
    }

    fn request_remote_devices_from_sfu_if_older_than(state: &mut State, max_age: Duration) {
        debug!(
            "group_call::Client::request_remote_devices_from_sfu_if_older_than(client_id: {}, max_age: {:?})",
            state.client_id, max_age
        );

        Self::maybe_request_remote_devices(state, max_age, false);
    }

    fn maybe_request_remote_devices(
        state: &mut State,
        max_age: Duration,
        rerequest_if_pending: bool,
    ) {
        let now = Instant::now();
        let should_request_now = match state.remote_devices_request_state {
            RemoteDevicesRequestState::WaitingForMembershipProof => false,
            RemoteDevicesRequestState::NeverRequested => true,
            RemoteDevicesRequestState::Requested {
                at: request_time, ..
            } => {
                // Timeout if we don't get a response
                now > request_time + Duration::from_secs(5)
            }
            RemoteDevicesRequestState::Updated { at: update_time } => now >= update_time + max_age,
            RemoteDevicesRequestState::Failed { at: failure_time } => {
                // Don't hammer server during failures
                now > failure_time + Duration::from_secs(5)
            }
        };
        if should_request_now {
            // We've already requested, so just wait until the next update and then request again.
            debug!("Request remote devices now.");
            let actor = state.actor.clone();
            state.sfu_client.peek(Box::new(move |peek_info| {
                actor.send(move |state| {
                    Self::set_peek_result_inner(state, peek_info);
                });
            }));
            state.remote_devices_request_state = RemoteDevicesRequestState::Requested {
                should_request_again: false,
                at: Instant::now(),
            };
        } else if rerequest_if_pending {
            // We've already requested, so just wait until the next update and then request again.
            debug!("Request remote devices later because there's a request pending.");
            if let RemoteDevicesRequestState::Requested { at, .. } =
                state.remote_devices_request_state
            {
                state.remote_devices_request_state = RemoteDevicesRequestState::Requested {
                    at,
                    should_request_again: true,
                }
            }
        } else {
            debug!("Just skip this request for remote devices.");
        }
    }

    pub fn connect(&self) {
        debug!(
            "group_call::Client(outer)::connect(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::connect(client_id: {})",
                state.client_id
            );

            match state.connection_state {
                ConnectionState::Connected | ConnectionState::Reconnecting => {
                    warn!("Can't connect when already connected.");
                }
                ConnectionState::Connecting => {
                    warn!("Can't connect when already connecting.");
                }
                ConnectionState::NotConnected => {
                    // Because the SfuClient currently doesn't allow connecting without joining,
                    // we just pretend to connect and wait for join() to be called.
                    Self::set_connection_state_and_notify_observer(
                        state,
                        ConnectionState::Connecting,
                    );

                    let now = Instant::now();

                    // Start heartbeats and audio levels right away.
                    state.next_heartbeat_time = Some(now);
                    state.next_audio_levels_time = Some(now);

                    // Request group membership refresh as we start polling the participant list.
                    if state.kind == GroupCallKind::SignalGroup {
                        state.observer.request_membership_proof(state.client_id);
                        state.next_membership_proof_request_time =
                            Some(now + MEMBERSHIP_PROOF_REQUEST_INTERVAL);

                        // Request the list of all group members
                        state.observer.request_group_members(state.client_id);
                    }

                    Self::tick(state);
                }
            }
        });
    }

    // Pulled into a named private method because it might be called by many methods.
    fn set_connection_state_and_notify_observer(
        state: &mut State,
        connection_state: ConnectionState,
    ) {
        debug!(
            "group_call::Client(inner)::set_connection_state_and_notify_observer(client_id: {})",
            state.client_id
        );

        state.connection_state = connection_state;
        state
            .observer
            .handle_connection_state_changed(state.client_id, connection_state);
    }

    // Pulled into a private method so we can lock/set/unlock the busy state.
    fn take_busy(state: &mut State) -> bool {
        let busy = state.busy.lock();
        match busy {
            Ok(mut busy) => {
                if *busy {
                    info!("Call Manager is busy with another call");
                    false
                } else {
                    *busy = true;
                    true
                }
            }
            Err(err) => {
                error!("Can't lock busy: {}", err);
                false
            }
        }
    }

    fn release_busy(state: &mut State) {
        let busy = state.busy.lock();
        match busy {
            Ok(mut busy) => {
                *busy = false;
            }
            Err(err) => {
                error!("Can't lock busy: {}", err);
            }
        }
    }

    pub fn join(&self) {
        debug!(
            "group_call::Client(outer)::join(client_id: {})",
            self.client_id
        );
        let callback = self.clone();
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::join(client_id: {})",
                state.client_id
            );
            match state.join_state {
                JoinState::Joined(_) => {
                    warn!("Can't join when already joined.");
                }
                JoinState::Joining => {
                    warn!("Can't join when already joining.");
                }
                JoinState::NotJoined(ring_id) => {
                    if let Some(peek_info) = &state.last_peek_info {
                        if peek_info.device_count() >= peek_info.max_devices.unwrap_or(u32::MAX) as usize {
                            info!("Ending group call client because there are {}/{} devices in the call.", peek_info.device_count(), peek_info.max_devices.unwrap());
                            Self::end(state, EndReason::HasMaxDevices);
                            return;
                        }
                    }
                    if Self::take_busy(state) {
                        Self::set_join_state_and_notify_observer(state, JoinState::Joining);
                        Self::accept_ring_if_needed(state, ring_id);

                        if state.kind == GroupCallKind::SignalGroup {
                            // Request group membership refresh before joining.
                            // The Join request will then proceed once SfuClient has the token.
                            state.observer.request_membership_proof(state.client_id);
                            state.next_membership_proof_request_time = Some(Instant::now() + MEMBERSHIP_PROOF_REQUEST_INTERVAL);
                        }

                        let client_secret = EphemeralSecret::new(OsRng);
                        let client_pub_key = PublicKey::from(&client_secret);
                        state.dhe_state = DheState::start(client_secret);
                        state.sfu_client.join(
                            &state.local_ice_ufrag,
                            *client_pub_key.as_bytes(),
                            callback,
                        );
                    } else {
                        Self::end(state, EndReason::CallManagerIsBusy);
                    }
                }
            }
        });
    }

    fn accept_ring_if_needed(state: &mut State, ring_id: Option<RingId>) {
        if let Some(ring_id) = ring_id {
            if let Some(self_uuid) = state.self_uuid.lock().expect("can read UUID").clone() {
                let accept_message = protobuf::signaling::CallMessage {
                    ring_response: Some(protobuf::signaling::call_message::RingResponse {
                        group_id: Some(state.group_id.clone()),
                        ring_id: Some(ring_id.into()),
                        r#type: Some(
                            protobuf::signaling::call_message::ring_response::Type::Accepted.into(),
                        ),
                    }),
                    ..Default::default()
                };

                state.observer.send_signaling_message(
                    self_uuid,
                    accept_message,
                    SignalingMessageUrgency::HandleImmediately,
                );
            } else {
                error!("self UUID unknown; cannot notify other devices of accept");
            }
        }
    }

    // Pulled into a named private method because it might be called by leave_inner().
    fn set_join_state_and_notify_observer(state: &mut State, join_state: JoinState) {
        debug!(
            "group_call::Client(inner)::set_join_state_and_notify_observer(client_id: {}, join_state: {:?})",
            state.client_id,
            join_state
        );
        state.join_state = join_state;
        state
            .observer
            .handle_join_state_changed(state.client_id, join_state);
    }

    pub fn leave(&self) {
        debug!(
            "group_call::Client(outer)::leave(client_id: {})",
            self.client_id
        );
        self.actor.send(Self::leave_inner);
    }

    // Pulled into a named private method because it might be called by end().
    fn leave_inner(state: &mut State) {
        debug!(
            "group_call::Client(inner)::leave(client_id: {}, join_state: {:?})",
            state.client_id, state.join_state
        );

        Self::cancel_full_group_ring_if_needed(state);

        match state.join_state {
            JoinState::NotJoined(_) => {
                warn!("Can't leave when not joined.");
            }
            JoinState::Joining | JoinState::Joined(_) => {
                state.peer_connection.set_outgoing_media_enabled(false);
                state.peer_connection.set_incoming_media_enabled(false);
                Self::release_busy(state);

                if let JoinState::Joined(local_demux_id) = state.join_state {
                    Self::send_leaving_through_sfu_and_over_signaling(state, local_demux_id);
                    Self::send_leave_to_sfu(state);
                }
                Self::set_join_state_and_notify_observer(state, JoinState::NotJoined(None));
                state.next_heartbeat_time = None;
                state.next_stats_time = None;
                state.next_audio_levels_time = None;
                state.next_membership_proof_request_time = None;
                state.has_ever_been_participating_client = false;
            }
        }
    }

    pub fn disconnect(&self) {
        debug!(
            "group_call::Client(outer)::disconnect(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::disconnect(client_id: {})",
                state.client_id
            );
            Self::end(state, EndReason::DeviceExplicitlyDisconnected);
        });
    }

    pub fn ring(&self, recipient: Option<UserId>) {
        debug!(
            "group_call::Client(outer)::ring(client_id: {}, recipient: {:?})",
            self.client_id, recipient,
        );
        self.actor
            .send(move |state| Self::ring_inner(state, recipient));
    }

    fn ring_inner(state: &mut State, recipient: Option<UserId>) {
        debug!(
            "group_call::Client(inner)::ring(client_id: {}, recipient: {:?})",
            state.client_id, recipient
        );

        match state.outgoing_ring_state {
            OutgoingRingState::PermittedToRing { ring_id } => {
                let message = protobuf::signaling::CallMessage {
                    ring_intention: Some(protobuf::signaling::call_message::RingIntention {
                        group_id: Some(state.group_id.clone()),
                        ring_id: Some(ring_id.into()),
                        r#type: Some(
                            protobuf::signaling::call_message::ring_intention::Type::Ring.into(),
                        ),
                    }),
                    ..Default::default()
                };

                if recipient.is_some() {
                    unimplemented!("cannot ring just one person yet");
                } else {
                    state.observer.send_signaling_message_to_group(
                        state.group_id.clone(),
                        message,
                        SignalingMessageUrgency::HandleImmediately,
                    );

                    if state.remote_devices.is_empty() {
                        // If you're the only one in the call at the time of the ring,
                        // and then you leave before anyone joins, the ring is auto-cancelled.
                        state.outgoing_ring_state = OutgoingRingState::HasSentRing { ring_id };
                    } else {
                        // Otherwise, the ring is sent-and-forgotten.
                        state.outgoing_ring_state = OutgoingRingState::NotPermittedToRing;
                    }
                }
            }
            OutgoingRingState::WantsToRing { .. } => {
                warn!(
                    "repeat ring request not supported (client_id: {}, ring not yet sent)",
                    state.client_id
                );
            }
            OutgoingRingState::HasSentRing { ring_id, .. } => {
                warn!(
                    "repeat ring request not supported (client_id: {}, previous ring id: {})",
                    state.client_id, ring_id
                );
            }
            OutgoingRingState::Unknown => {
                // Need to wait until joining
                state.outgoing_ring_state = OutgoingRingState::WantsToRing { recipient };
            }
            OutgoingRingState::NotPermittedToRing => {
                info!(
                    "ringing is not permitted (client_id: {}); most likely someone else started the call first",
                    state.client_id
                );
            }
        }
    }

    pub fn set_outgoing_audio_muted(&self, muted: bool) {
        debug!(
            "group_call::Client(outer)::set_audio_muted(client_id: {}, muted: {})",
            self.client_id, muted
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::set_audio_muted(client_id: {}, muted: {})",
                state.client_id, muted
            );
            // We don't modify the outgoing audio track.  We expect the app to handle that.
            state.outgoing_heartbeat_state.audio_muted = Some(muted);
            if let Err(err) = Self::send_heartbeat(state) {
                warn!(
                    "Failed to send heartbeat after updating audio mute state: {:?}",
                    err
                );
            }
        });
    }

    pub fn set_outgoing_video_muted(&self, muted: bool) {
        debug!(
            "group_call::Client(outer)::set_video_muted(client_id: {}, muted: {})",
            self.client_id, muted
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::set_video_muted(client_id: {}, muted: {})",
                state.client_id, muted
            );
            // We don't modify the outgoing video track.  We expect the app to handle that.
            state.outgoing_heartbeat_state.video_muted = Some(muted);
            if let Err(err) = Self::send_heartbeat(state) {
                warn!(
                    "Failed to send heartbeat after updating video mute state: {:?}",
                    err
                );
            }
        });
    }

    pub fn set_presenting(&self, presenting: bool) {
        debug!(
            "group_call::Client(outer)::set_presenting(client_id: {}, presenting: {})",
            self.client_id, presenting
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::set_presenting(client_id: {}, presenting: {})",
                state.client_id, presenting
            );
            state.outgoing_heartbeat_state.presenting = Some(presenting);
            if let Err(err) = Self::send_heartbeat(state) {
                warn!(
                    "Failed to send heartbeat after updating presenting state: {:?}",
                    err
                );
            }
        });
    }

    pub fn set_sharing_screen(&self, sharing_screen: bool) {
        debug!(
            "group_call::Client(outer)::set_sharing_screen(client_id: {}, sharing_screen: {})",
            self.client_id, sharing_screen
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::set_sharing_screen(client_id: {}, sharing_screen: {})",
                state.client_id, sharing_screen
            );
            state.outgoing_heartbeat_state.sharing_screen = Some(sharing_screen);
            if let Err(err) = Self::send_heartbeat(state) {
                warn!(
                    "Failed to send heartbeat after updating sharing screen state: {:?}",
                    err
                );
            }
            let send_rates = Self::compute_send_rates(state.joined_members.len(), sharing_screen);
            Self::set_send_rates_inner(state, send_rates);
        });
    }

    pub fn resend_media_keys(&self) {
        debug!(
            "group_call::Client(outer)::resend_media_keys(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::resend_media_keys(client_id: {})",
                state.client_id
            );

            if let JoinState::Joined(local_demux_id) = state.join_state {
                let user_ids: HashSet<UserId> = state
                    .remote_devices
                    .iter()
                    .map(|rd| rd.user_id.clone())
                    .collect();

                let (ratchet_counter, secret) = {
                    let frame_crypto_context = state
                        .frame_crypto_context
                        .lock()
                        .expect("Get lock for frame encryption context to advance media send key");
                    frame_crypto_context.send_state()
                };

                info!(
                    "Resending media keys to everyone (number of users: {})",
                    user_ids.len()
                );
                for user_id in user_ids {
                    Self::send_media_send_key_to_user_over_signaling(
                        state,
                        user_id,
                        local_demux_id,
                        ratchet_counter,
                        secret,
                    );
                }
            }
        });
    }

    pub fn set_data_mode(&self, data_mode: DataMode) {
        debug!(
            "group_call::Client(outer)::set_data_mode(client_id: {}, data_mode: {:?})",
            self.client_id, data_mode
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::set_data_mode(client_id: {}), data_mode: {:?}",
                state.client_id, data_mode,
            );

            state.max_receive_rate = Some(match data_mode {
                DataMode::Low => LOW_MAX_RECEIVE_RATE,
                DataMode::Normal => NORMAL_MAX_RECEIVE_RATE,
            });

            state.data_mode = data_mode;
            match state.join_state {
                JoinState::NotJoined(_) | JoinState::Joining => {
                    // The audio encoders will be configured with data_mode upon joining.
                }
                JoinState::Joined(_) => {
                    state
                        .peer_connection
                        .configure_audio_encoders(&data_mode.audio_encoder_config());
                }
            };

            if !state.on_demand_video_request_sent_since_last_heartbeat {
                Self::send_video_requests_to_sfu(state);
                state.on_demand_video_request_sent_since_last_heartbeat = true;
            }
        });
    }

    fn set_send_rates_inner(state: &mut State, mut send_rates: SendRates) {
        if let Some(send_rates_override) = &state.send_rates_override {
            send_rates = send_rates_override.clone();
        }
        if state.send_rates != send_rates {
            if send_rates.max == Some(ALL_ALONE_MAX_SEND_RATE) {
                info!("Disable audio and outgoing media because there are no other devices.");
                state.peer_connection.set_audio_recording_enabled(false);
                state.peer_connection.set_audio_playout_enabled(false);
                state.peer_connection.set_outgoing_media_enabled(false);
            } else {
                info!("Enable audio and outgoing media because there are other devices.");
                state.peer_connection.set_audio_recording_enabled(true);
                state.peer_connection.set_audio_playout_enabled(true);
                state.peer_connection.set_outgoing_media_enabled(true);
            }
            if let Err(e) = state.peer_connection.set_send_rates(send_rates.clone()) {
                warn!("Could not set send rates to {:?}: {}", send_rates, e);
            } else {
                info!("Setting send rates to {:?}", send_rates);
                state
                    .observer
                    .handle_send_rates_changed(state.client_id, send_rates);
            }
        }
    }

    pub fn request_video(&self, requests: Vec<VideoRequest>, active_speaker_height: u16) {
        debug!(
            "group_call::Client(outer)::request_video(client_id: {}, requests: {:?}, active_speaker_height: {})",
            self.client_id, requests, active_speaker_height,
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::request_video(client_id: {})",
                state.client_id
            );
            state.video_requests = Some(requests);
            state.active_speaker_height = Some(active_speaker_height);
            if !state.on_demand_video_request_sent_since_last_heartbeat {
                Self::send_video_requests_to_sfu(state);
                state.on_demand_video_request_sent_since_last_heartbeat = true;
            }
        });
    }

    fn send_video_requests_to_sfu(state: &mut State) {
        use protobuf::group_call::{
            device_to_sfu::{
                video_request_message::VideoRequest as VideoRequestProto, VideoRequestMessage,
            },
            DeviceToSfu,
        };
        use std::cmp::min;

        if let Some(video_requests) = &state.video_requests {
            let requests: Vec<_> = video_requests
                .iter()
                .filter_map(|request| {
                    state
                        .remote_devices
                        .find_by_demux_id(request.demux_id)
                        .map(|device| {
                            VideoRequestProto {
                                demux_id: Some(device.demux_id),
                                // We use the min because the SFU does not understand the concept of video rotation
                                // so all requests must be in terms of non-rotated video even though the apps
                                // will request in terms of rotated video.  We assume that all video is sent over the
                                // wire in landscape format with rotation metadata.
                                // If it's not, we'll have a problem.
                                height: Some(min(request.height, request.width) as u32),
                            }
                        })
                })
                .collect();
            let msg = DeviceToSfu {
                video_request: Some(VideoRequestMessage {
                    // TODO: Update the server to handle this as expected or remove this altogether.
                    // The client needs the server to sort by resolution and then cap the number after that sort.
                    // Currently, the server is sorting by audio activity and then capping the number.
                    // Two possible fixes on the server:
                    // A. Sort by resolution and then cap.
                    //    After that, the client could re-add the lines below.
                    // B. Treat the list of resolution requests as "complete" and don't use "lastN" at all.
                    //    After that, the client could remove the lines below.
                    // Note: the server can't handle a None value here, so we have to pass
                    // in a value larger than a group call would ever be.
                    // The only problem with this mechanism is that the server will send video for
                    // new remote devices that the local device hasn't yet learned about.
                    // max: Some(
                    //     requests
                    //         .iter()
                    //         .filter(|request| request.height.unwrap() > 0)
                    //         .count() as u32,
                    // ),
                    max_kbps: state.max_receive_rate.map(|rate| rate.as_kbps() as u32),
                    requests,
                    active_speaker_height: state.active_speaker_height.map(|height| height.into()),
                }),
                ..Default::default()
            };

            if let Err(e) = Self::send_data_to_sfu(state, &msg.encode_to_vec()) {
                warn!("Failed to send video request: {:?}", e);
            }
        }
    }

    fn approve_or_deny_user(state: &mut State, user_id: UserId, approved: bool) {
        use protobuf::group_call::{
            device_to_sfu::{AdminAction, GenericAdminAction},
            DeviceToSfu,
        };

        // Approval is implemented by demux ID (because we don't put user IDs in RTP messages).
        // So we have to find a corresponding demux ID in the pending users list.
        let Some(peek_info) = state.last_peek_info.as_ref() else {
            error!("Cannot approve users without peek info");
            return;
        };

        let action_to_log = if approved { "approval" } else { "denial" };

        if let Some(demux_id) = peek_info
            .pending_devices
            .iter()
            .find(|device| device.user_id.as_ref() == Some(&user_id))
            .map(|device| device.demux_id)
        {
            let action = if approved {
                AdminAction::Approve
            } else {
                AdminAction::Deny
            };
            let msg = DeviceToSfu {
                admin_action: Some((action)(GenericAdminAction {
                    target_demux_id: Some(demux_id),
                })),
                ..Default::default()
            };

            if let Err(e) = Self::send_data_to_sfu(state, &msg.encode_to_vec()) {
                warn!("Failed to send {}: {:?}", action_to_log, e);
            }
        } else if let Some(demux_id) = peek_info
            .devices
            .iter()
            .find(|device| device.user_id.as_ref() == Some(&user_id))
            .map(|device| device.demux_id)
        {
            info!("User has already been added to call with demux ID {demux_id}");
        } else {
            warn!("Failed to find user for {action_to_log} (they may have left or been denied by another admin)");
        }
    }

    pub fn approve_user(&self, user_id: UserId) {
        debug!(
            "group_call::Client(outer)::approve_user(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::approve_user(client_id: {})",
                state.client_id
            );
            Self::approve_or_deny_user(state, user_id, true);
        });
    }

    pub fn deny_user(&self, user_id: UserId) {
        debug!(
            "group_call::Client(outer)::deny_user(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::deny_user(client_id: {})",
                state.client_id
            );
            Self::approve_or_deny_user(state, user_id, false);
        });
    }

    pub fn remove_client(&self, other_client: DemuxId) {
        use protobuf::group_call::{
            device_to_sfu::{AdminAction, GenericAdminAction},
            DeviceToSfu,
        };
        debug!(
            "group_call::Client(outer)::remove_client(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::remove_client(client_id: {})",
                state.client_id
            );

            // We could check that other_client is a valid demux ID according to our current peek
            // info, but that's a racy check anyway. Just let the calling server do it.
            let msg = DeviceToSfu {
                admin_action: Some(AdminAction::Remove(GenericAdminAction {
                    target_demux_id: Some(other_client),
                })),
                ..Default::default()
            };

            if let Err(e) = Self::send_data_to_sfu(state, &msg.encode_to_vec()) {
                warn!("Failed to send removal: {:?}", e);
            }
        });
    }

    // Blocks are performed on a particular client, but end up affecting all of the user's devices.
    // Still, we define it as a demux-ID-based operation for more flexibility later.
    pub fn block_client(&self, other_client: DemuxId) {
        use protobuf::group_call::{
            device_to_sfu::{AdminAction, GenericAdminAction},
            DeviceToSfu,
        };
        debug!(
            "group_call::Client(outer)::block_client(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::block_client(client_id: {})",
                state.client_id
            );

            // We could check that other_client is a valid demux ID according to our current peek
            // info, but that's a racy check anyway. Just let the calling server do it.
            let msg = DeviceToSfu {
                admin_action: Some(AdminAction::Block(GenericAdminAction {
                    target_demux_id: Some(other_client),
                })),
                ..Default::default()
            };

            if let Err(e) = Self::send_data_to_sfu(state, &msg.encode_to_vec()) {
                warn!("Failed to send block: {:?}", e);
            }
        });
    }

    pub fn set_group_members(&self, group_members: Vec<GroupMember>) {
        debug!(
            "group_call::Client(outer)::set_group_members(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::set_group_members(client_id: {})",
                state.client_id
            );
            let new_members: HashSet<UserId> =
                group_members.iter().map(|i| i.user_id.clone()).collect();
            if new_members != state.known_members {
                info!("known group members changed");
                state.known_members = new_members;
                state.sfu_client.set_group_members(group_members);
                Self::request_remote_devices_as_soon_as_possible(state);
            }
        })
    }

    pub fn set_membership_proof(&self, proof: MembershipProof) {
        debug!(
            "group_call::Client(outer)::set_membership_proof(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::set_membership_proof(client_id: {})",
                state.client_id
            );
            state.sfu_client.set_membership_proof(proof);
            if matches!(
                state.remote_devices_request_state,
                RemoteDevicesRequestState::WaitingForMembershipProof
            ) {
                state.remote_devices_request_state = RemoteDevicesRequestState::NeverRequested;
                Self::request_remote_devices_as_soon_as_possible(state);
            }
        })
    }

    // Pulled into a named private method because it can be called in many places.
    #[allow(clippy::collapsible_if)]
    fn end(state: &mut State, reason: EndReason) {
        debug!(
            "group_call::Client(inner)::end(client_id: {})",
            state.client_id
        );

        let joining_or_joined = match state.join_state {
            JoinState::Joined(_) | JoinState::Joining => true,
            JoinState::NotJoined(_) => false,
        };
        if joining_or_joined {
            // This will send an update after changing the join state.
            Self::leave_inner(state);
        }
        match state.connection_state {
            ConnectionState::NotConnected => {
                warn!("Can't disconnect when not connected.");
            }
            ConnectionState::Connecting
            | ConnectionState::Connected
            | ConnectionState::Reconnecting => {
                state.peer_connection.close();
                Self::set_connection_state_and_notify_observer(
                    state,
                    ConnectionState::NotConnected,
                );
                let _join_handles = state.actor.stopper().stop_all_without_joining();
                state.observer.handle_ended(state.client_id, reason);
            }
        }
    }

    // This should be called by the SfuClient after it has joined.
    pub fn on_sfu_client_joined(&self, joined: Result<Joined>) {
        debug!(
            "group_call::Client(outer)::on_sfu_client_joined(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::on_sfu_client_joined(client_id: {})",
                state.client_id
            );

            if let Ok(Joined {
                sfu_info,
                local_demux_id,
                server_dhe_pub_key,
                hkdf_extra_info,
                creator,
                era_id,
            }) = joined
            {
                match state.connection_state {
                    ConnectionState::NotConnected => {
                        warn!("The SFU completed joining before connect() was requested.");
                    }
                    ConnectionState::Connecting => {
                        state.dhe_state.negotiate_in_place(
                            &PublicKey::from(server_dhe_pub_key),
                            &hkdf_extra_info,
                        );
                        let srtp_keys = match &state.dhe_state {
                            DheState::Negotiated { srtp_keys } => srtp_keys,
                            _ => {
                                Self::end(state, EndReason::FailedToNegotiatedSrtpKeys);
                                return;
                            }
                        };

                        if Self::start_peer_connection(state, &sfu_info, local_demux_id, srtp_keys)
                            .is_err()
                        {
                            Self::end(state, EndReason::FailedToStartPeerConnection);
                            return;
                        };

                        // Set a low bitrate until we learn someone else is in the call.
                        Self::set_send_rates_inner(
                            state,
                            SendRates {
                                max: Some(ALL_ALONE_MAX_SEND_RATE),
                                ..SendRates::default()
                            },
                        );

                        state.sfu_info = Some(sfu_info);
                    }
                    ConnectionState::Connected | ConnectionState::Reconnecting => {
                        warn!("The SFU completed joining after already being connected.");
                    }
                };
                match state.join_state {
                    JoinState::NotJoined(_) => {
                        warn!("The SFU completed joining before join() was requested.");
                    }
                    JoinState::Joining => {
                        // The call to set_peek_result_inner needs the join state to be joined.
                        // But make sure to fire observer.handle_join_state_changed after
                        // set_peek_result_inner so that state.remote_devices are filled in.
                        state.join_state = JoinState::Joined(local_demux_id);
                        if let Some(peek_info) = &state.last_peek_info {
                            // TODO: Do the same processing without making it look like we just
                            // got an update from the server even though the update actually came
                            // from earlier.  For now, it's close enough.
                            let peek_info = peek_info.clone();
                            Self::set_peek_result_inner(state, Ok(peek_info));
                            if state.remote_devices.is_empty() {
                                // If there are no remote devices, then Self::set_peek_result_inner
                                // will not fire handle_remote_devices_changed and the observer can't tell the difference
                                // between "we know we have no remote devices" and "we don't know what we have yet".
                                // This way, the observer can.
                                state.observer.handle_remote_devices_changed(
                                    state.client_id,
                                    &state.remote_devices,
                                    RemoteDevicesChangedReason::DemuxIdsChanged,
                                );
                            }
                        }
                        state
                            .observer
                            .handle_join_state_changed(state.client_id, state.join_state);

                        if creator.is_some() {
                            // Check if we're permitted to ring
                            let creator_is_self = {
                                let self_uuid_guard = state.self_uuid.lock();
                                self_uuid_guard
                                    .map(|guarded_uuid| creator == *guarded_uuid)
                                    .unwrap_or(false)
                            };
                            let new_ring_state = if creator_is_self {
                                OutgoingRingState::PermittedToRing {
                                    ring_id: RingId::from_era_id(&era_id),
                                }
                            } else {
                                OutgoingRingState::NotPermittedToRing
                            };
                            debug!("updating ring state to {:?}", new_ring_state);
                            let previous_ring_state =
                                std::mem::replace(&mut state.outgoing_ring_state, new_ring_state);
                            if let OutgoingRingState::WantsToRing { recipient } =
                                previous_ring_state
                            {
                                Self::ring_inner(state, recipient)
                            }
                        }

                        // We just now appeared in the participants list, and possibly even updated
                        // the eraId.
                        Self::request_remote_devices_as_soon_as_possible(state);
                        state.next_stats_time = Some(Instant::now() + STATS_INITIAL_OFFSET);

                        state
                            .peer_connection
                            .configure_audio_encoders(&state.data_mode.audio_encoder_config());
                    }
                    JoinState::Joined(_) => {
                        warn!("The SFU completed joining more than once.");
                    }
                };
            } else {
                Self::end(state, EndReason::SfuClientFailedToJoin);
            }
        });
    }

    pub fn on_signaling_message_received(
        &self,
        sender_user_id: UserId,
        message: protobuf::group_call::DeviceToDevice,
    ) {
        debug!(
            "group_call::Client(outer)::on_signaling_message_received(client_id: {})",
            self.client_id
        );
        self.actor.send(move |state| {
            debug!(
                "group_call::Client(inner)::on_signaling_message_received(client_id: {})",
                state.client_id
            );
            match message {
                protobuf::group_call::DeviceToDevice {
                    media_key:
                        Some(protobuf::group_call::device_to_device::MediaKey {
                            demux_id: Some(sender_demux_id),
                            ratchet_counter: Some(ratchet_counter),
                            secret: Some(secret_vec),
                            ..
                        }),
                    ..
                } => {
                    if secret_vec.len() != size_of::<frame_crypto::Secret>() {
                        warn!("on_signaling_message_received(): ignoring media receive key with wrong length");
                        return;
                    }
                    if let Ok(ratchet_counter) = ratchet_counter.try_into() {
                        let mut secret = frame_crypto::Secret::default();
                        secret.copy_from_slice(&secret_vec);
                        Self::add_media_receive_key_or_store_for_later(
                            state,
                            sender_user_id,
                            sender_demux_id,
                            ratchet_counter,
                            secret,
                        );
                    } else {
                        warn!("on_signaling_message_received(): ignoring media receive key with ratchet counter that's too big");
                    }
                    let known = state.remote_devices.iter().any(|rd| rd.demux_id == sender_demux_id);
                    if !known {
                        // It's likely someone this demux ID just joined.
                        debug!("Request devices because we receive a signaling message from unknown demux_id = {}", sender_demux_id);
                        Self::request_remote_devices_as_soon_as_possible(state);
                    }
                }
                protobuf::group_call::DeviceToDevice {
                    group_id: Some(group_id),
                    leaving: Some(protobuf::group_call::device_to_device::Leaving {
                        demux_id: Some(leaving_demux_id),
                        ..
                    }),
                    ..
                } => {
                    if group_id == state.group_id {
                        Self::handle_leaving_received(state, leaving_demux_id);
                    }
                }
                _ => {
                    warn!("on_signaling_message_received(): ignoring unknown message");
                }
            }
        });
    }

    // Pulled into a named private method because it's more convenient to deal with errors that way
    fn start_peer_connection(
        state: &State,
        sfu_info: &SfuInfo,
        local_demux_id: DemuxId,
        srtp_keys: &SrtpKeys,
    ) -> Result<()> {
        debug!(
            "group_call::Client(inner)::start_peer_connection(client_id: {})",
            state.client_id
        );

        Self::set_peer_connection_descriptions(state, sfu_info, local_demux_id, &[], srtp_keys)?;

        for addr in &sfu_info.udp_addresses {
            // We use the octets instead of to_string() to bypass the IP address logging filter.
            info!(
                "Connecting to group call SFU via UDP with ip={:?} port={}",
                match addr.ip() {
                    std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
                    std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
                },
                addr.port()
            );
            state.peer_connection.add_ice_candidate_from_server(
                addr.ip(),
                addr.port(),
                false, /* tcp */
            )?;
        }

        for addr in &sfu_info.tcp_addresses {
            // We use the octets instead of to_string() to bypass the IP address logging filter.
            info!(
                "Connecting to group call SFU via TCP with ip={:?} port={}",
                match addr.ip() {
                    std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
                    std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
                },
                addr.port()
            );
            state.peer_connection.add_ice_candidate_from_server(
                addr.ip(),
                addr.port(),
                true, /* tcp */
            )?;
        }

        if state
            .peer_connection
            .receive_rtp(RTP_DATA_PAYLOAD_TYPE)
            .is_err()
        {
            warn!("Could not tell PeerConnection to receive RTP");
        }

        Ok(())
    }

    pub fn set_peek_result(&self, result: PeekResult) {
        debug!(
            "group_call::Client(outer)::set_peek_result: {}, result: {:?})",
            self.client_id, result
        );

        self.actor.send(move |state| {
            Self::set_peek_result_inner(state, result);
        });
    }

    // Most of the logic moved to inner method so this can be called by both
    // set_peek_result() and as a callback to SfuClient::request_remote_devices.
    fn set_peek_result_inner(state: &mut State, result: PeekResult) {
        debug!(
            "group_call::Client(inner)::set_peek_result_inner(client_id: {}, result: {:?} state: {:?})",
            state.client_id, result, state.remote_devices_request_state
        );

        if let Err(e) = result {
            warn!("Failed to request remote devices from SFU: {:?}", e);
            state.remote_devices_request_state =
                RemoteDevicesRequestState::Failed { at: Instant::now() };
            return;
        }
        let peek_info = result.unwrap();

        let is_first_peek_info = state.last_peek_info.is_none();
        let should_request_again = matches!(
            state.remote_devices_request_state,
            RemoteDevicesRequestState::Requested {
                should_request_again: true,
                ..
            }
        );
        state.remote_devices_request_state =
            RemoteDevicesRequestState::Updated { at: Instant::now() };

        let old_user_ids: HashSet<UserId> = std::mem::take(&mut state.joined_members);
        let new_user_ids: HashSet<UserId> = peek_info
            .devices
            .iter()
            // Note: this ignores users that aren't in the group
            .filter_map(|device| device.user_id.clone())
            .collect();

        // When would this combined hash falsely claim that the set of pending users hasn't changed?
        // If the combined hash of the user IDs that have been added and removed since the last peek
        // comes out to the exact bit-pattern needed to match the change in `pending_devices.len()`.
        // For example, if one person left and one person joined the pending list, their user IDs
        // would have to have hashes of `x` and `-x`, so that combined they equal 0. This is
        // extremely unlikely.
        let new_pending_users_signature = peek_info
            .unique_pending_users()
            .into_iter()
            .map(|user_id| {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                user_id.hash(&mut hasher);
                hasher.finish()
            })
            .fold(peek_info.pending_devices.len() as u64, |a, b| {
                // Note that this is an order-independent fold, so that two differently-ordered
                // HashSets produce the same signature.
                a.wrapping_add(b)
            });

        let old_era_id = state
            .last_peek_info
            .as_ref()
            .and_then(|peek_info| peek_info.era_id.as_ref());

        if is_first_peek_info
            || old_user_ids != new_user_ids
            || old_era_id != peek_info.era_id.as_ref()
            || state.pending_users_signature != new_pending_users_signature
        {
            state
                .observer
                .handle_peek_changed(state.client_id, &peek_info, &new_user_ids)
        }

        if let (JoinState::Joined(local_demux_id), DheState::Negotiated { srtp_keys }) =
            (&state.join_state, &state.dhe_state)
        {
            let local_demux_id = *local_demux_id;
            // We remember these before changing state.remote_devices so we can calculate changes after.
            let old_demux_ids: HashSet<DemuxId> = state.remote_devices.demux_id_set();

            // Then we update state.remote_devices by first building a map of demux_id => RemoteDeviceState
            // from the old values and then building a new Vec using either the old value (if there is one)
            // or creating a new one.
            let mut old_remote_devices_by_demux_id: HashMap<DemuxId, RemoteDeviceState> =
                std::mem::take(&mut state.remote_devices)
                    .into_iter()
                    .map(|rd| (rd.demux_id, rd))
                    .collect();
            let added_time = SystemTime::now();
            state.remote_devices = peek_info
                .devices
                .iter()
                .filter_map(|device| {
                    if device.demux_id == local_demux_id {
                        // Don't add a remote device to represent the local device.
                        state.has_ever_been_participating_client = true;
                        return None;
                    }
                    device.user_id.as_ref().map(|user_id| {
                        // Keep the old one, with its state, if there is one and the user ID
                        // matches.
                        if let Some(existing_remote_device) =
                            old_remote_devices_by_demux_id.remove(&device.demux_id)
                        {
                            if &existing_remote_device.user_id == user_id {
                                return existing_remote_device;
                            }
                        }
                        RemoteDeviceState::new(device.demux_id, user_id.clone(), added_time)
                    })
                })
                .collect();

            // Recalculate to see the differences
            let new_demux_ids: HashSet<DemuxId> = state.remote_devices.demux_id_set();

            let demux_ids_changed = old_demux_ids != new_demux_ids;
            // If demux IDs changed, let the PeerConnection know that related SSRCs changed as well
            if demux_ids_changed {
                info!(
                    "New set of demux IDs to be pushed down to PeerConnection: {:?}",
                    new_demux_ids
                );
                if let Some(sfu_info) = state.sfu_info.as_ref() {
                    let new_demux_ids: Vec<DemuxId> = new_demux_ids.iter().copied().collect();
                    let result = Self::set_peer_connection_descriptions(
                        state,
                        sfu_info,
                        local_demux_id,
                        &new_demux_ids,
                        srtp_keys,
                    );
                    if result.is_err() {
                        Self::end(state, EndReason::FailedToUpdatePeerConnection);
                        return;
                    }
                }
            }

            if demux_ids_changed {
                state.observer.handle_remote_devices_changed(
                    state.client_id,
                    &state.remote_devices,
                    RemoteDevicesChangedReason::DemuxIdsChanged,
                );
            }

            // If someone was added, we must advance the send media key
            // and send it to everyone that was added.
            let added_demux_ids: HashSet<DemuxId> =
                new_demux_ids.difference(&old_demux_ids).copied().collect();
            let users_with_added_devices: Vec<UserId> = state
                .remote_devices
                .iter()
                .filter(|device| added_demux_ids.contains(&device.demux_id))
                .map(|device| device.user_id.clone())
                .collect();
            if !users_with_added_devices.is_empty() {
                Self::advance_media_send_key_and_send_to_users_with_added_devices(
                    state,
                    &users_with_added_devices[..],
                );
                Self::send_pending_media_send_key_to_users_with_added_devices(
                    state,
                    &users_with_added_devices[..],
                );
            }

            // If someone was removed, we must reset the send media key and send it to everyone not removed.
            if old_user_ids.difference(&new_user_ids).next().is_some() {
                Self::rotate_media_send_key_and_send_to_users_not_removed(state);
            }

            // We can't gate this behind the demux IDs changing because a forged demux ID might
            // be in there already when the non-forged one comes in.
            let pending_receive_keys = std::mem::take(&mut state.pending_media_receive_keys);
            for (user_id, demux_id, ratchet_counter, secret) in pending_receive_keys {
                // If we the key is still pending, we'll just put this back into state.pending_media_receive_keys.
                Self::add_media_receive_key_or_store_for_later(
                    state,
                    user_id,
                    demux_id,
                    ratchet_counter,
                    secret,
                );
            }
            if new_demux_ids.len() != old_demux_ids.len() {
                let send_rates = Self::compute_send_rates(
                    new_demux_ids.len(),
                    state
                        .outgoing_heartbeat_state
                        .sharing_screen
                        .unwrap_or(false),
                );
                Self::set_send_rates_inner(state, send_rates);
            }

            // If anyone has joined besides us, we won't cancel the ring on leave.
            if !new_demux_ids.is_empty()
                && matches!(
                    state.outgoing_ring_state,
                    OutgoingRingState::HasSentRing { .. }
                )
            {
                state.outgoing_ring_state = OutgoingRingState::NotPermittedToRing;
            }
        }
        state.last_peek_info = Some(peek_info);

        // Do this later so that we can use new_user_ids above without running into
        // referencing issues
        state.joined_members = new_user_ids;
        state.pending_users_signature = new_pending_users_signature;

        if should_request_again {
            // Something occurred while we were waiting for this update.
            // We should request again.
            debug!("Request devices because we previously requested while a request was pending");
            Self::request_remote_devices_as_soon_as_possible(state);
        }
    }

    // Returns (min, start, max)
    fn compute_send_rates(joined_member_count: usize, sharing_screen: bool) -> SendRates {
        match (joined_member_count, sharing_screen) {
            (0, _) => SendRates {
                max: Some(ALL_ALONE_MAX_SEND_RATE),
                ..SendRates::default()
            },
            (_, true) => SendRates {
                min: Some(SCREENSHARE_MIN_SEND_RATE),
                start: Some(SCREENSHARE_START_SEND_RATE),
                max: Some(SCREENSHARE_MAX_SEND_RATE),
            },
            (1..=7, _) => SendRates {
                max: Some(SMALL_CALL_MAX_SEND_RATE),
                ..SendRates::default()
            },
            _ => SendRates {
                max: Some(LARGE_CALL_MAX_SEND_RATE),
                ..SendRates::default()
            },
        }
    }

    // Pulled into a named private method because it might be called by set_peek_result
    fn set_peer_connection_descriptions(
        state: &State,
        sfu_info: &SfuInfo,
        local_demux_id: DemuxId,
        remote_demux_ids: &[DemuxId],
        srtp_keys: &SrtpKeys,
    ) -> Result<()> {
        let local_description = SessionDescription::local_for_group_call(
            &state.local_ice_ufrag,
            &state.local_ice_pwd,
            &srtp_keys.client,
            Some(local_demux_id),
        )?;
        let observer = create_ssd_observer();
        state
            .peer_connection
            .set_local_description(observer.as_ref(), local_description);
        observer.get_result()?;

        let remote_description = SessionDescription::remote_for_group_call(
            &sfu_info.ice_ufrag,
            &sfu_info.ice_pwd,
            &srtp_keys.server,
            remote_demux_ids,
        )?;
        let observer = create_ssd_observer();
        state
            .peer_connection
            .set_remote_description(observer.as_ref(), remote_description);
        observer.get_result()?;
        Ok(())
    }

    fn rotate_media_send_key_and_send_to_users_not_removed(state: &mut State) {
        match state.media_send_key_rotation_state {
            KeyRotationState::Pending { secret, .. } => {
                info!("Waiting to generate a new media send key until after the pending one has been applied. client_id: {}", state.client_id);

                state.media_send_key_rotation_state = KeyRotationState::Pending {
                    secret,
                    needs_another_rotation: true,
                }
            }
            KeyRotationState::Applied => {
                info!("Generating a new random media send key because a user has been removed. client_id: {}", state.client_id);

                // First generate a new key, then wait some time, and then apply it.
                let ratchet_counter: frame_crypto::RatchetCounter = 0;
                let secret = frame_crypto::random_secret(&mut rand::rngs::OsRng);

                if let JoinState::Joined(local_demux_id) = state.join_state {
                    let user_ids: HashSet<UserId> = state
                        .remote_devices
                        .iter()
                        .map(|rd| rd.user_id.clone())
                        .collect();
                    info!(
                        "Sending newly rotated key to everyone (number of users: {})",
                        user_ids.len()
                    );
                    for user_id in user_ids {
                        Self::send_media_send_key_to_user_over_signaling(
                            state,
                            user_id,
                            local_demux_id,
                            ratchet_counter,
                            secret,
                        );
                    }
                }

                state.media_send_key_rotation_state = KeyRotationState::Pending {
                    secret,
                    needs_another_rotation: false,
                };
                state.actor.send_delayed(
                    Duration::from_secs(MEDIA_SEND_KEY_ROTATION_DELAY_SECS),
                    move |state| {
                        info!("Applying the new send key. client_id: {}", state.client_id);
                        {
                            let mut frame_crypto_context =
                                state.frame_crypto_context.lock().expect(
                                    "Get lock for frame encryption context to reset media send key",
                                );
                            frame_crypto_context.reset_send_ratchet(secret);
                        }

                        let needs_another_rotation = matches!(
                            state.media_send_key_rotation_state,
                            KeyRotationState::Pending {
                                needs_another_rotation: true,
                                ..
                            }
                        );
                        state.media_send_key_rotation_state = KeyRotationState::Applied;
                        if needs_another_rotation {
                            Self::rotate_media_send_key_and_send_to_users_not_removed(state);
                        }
                    },
                )
            }
        }
    }

    fn advance_media_send_key_and_send_to_users_with_added_devices(
        state: &mut State,
        users_with_added_devices: &[UserId],
    ) {
        info!(
            "Advancing current media send key because a user has been added. client_id: {}",
            state.client_id
        );

        let (ratchet_counter, secret) = {
            let mut frame_crypto_context = state
                .frame_crypto_context
                .lock()
                .expect("Get lock for frame encryption context to advance media send key");
            frame_crypto_context.advance_send_ratchet()
        };
        if let JoinState::Joined(local_demux_id) = state.join_state {
            info!(
                "Sending newly advanced key to users with added devices (number of users: {})",
                users_with_added_devices.len()
            );
            for user_id in users_with_added_devices {
                Self::send_media_send_key_to_user_over_signaling(
                    state,
                    user_id.to_vec(),
                    local_demux_id,
                    ratchet_counter,
                    secret,
                );
            }
        }
    }

    fn add_media_receive_key_or_store_for_later(
        state: &mut State,
        user_id: UserId,
        demux_id: DemuxId,
        ratchet_counter: frame_crypto::RatchetCounter,
        secret: frame_crypto::Secret,
    ) {
        if let Some(device) = state.remote_devices.find_by_demux_id_mut(demux_id) {
            if device.user_id == user_id {
                info!(
                    "Adding media receive key from {}. client_id: {}",
                    device.demux_id, state.client_id
                );
                {
                    let mut frame_crypto_context = state
                        .frame_crypto_context
                        .lock()
                        .expect("Get lock for frame encryption context to add media receive key");
                    frame_crypto_context.add_receive_secret(demux_id, ratchet_counter, secret);
                }
                let had_media_keys = std::mem::replace(&mut device.media_keys_received, true);
                if !had_media_keys {
                    state.observer.handle_remote_devices_changed(
                        state.client_id,
                        &state.remote_devices,
                        RemoteDevicesChangedReason::MediaKeyReceived(demux_id),
                    )
                }
            } else {
                warn!("Ignoring received media key from user because the demux ID {} doesn't make sense", demux_id);
                debug!("  user_id: {}", uuid_to_string(&user_id));
            }
        } else {
            info!(
                "Storing media receive key from {} because we don't know who they are yet.",
                demux_id
            );
            if state.pending_media_receive_keys.is_empty()
                && state.kind == GroupCallKind::SignalGroup
            {
                // Proactively ask for the group members again.
                // Since pending_media_receive_keys is re-processed every time we get a device
                // update, this will effectively be requested once per peek as long as there's an
                // unknown device in the call.
                state.observer.request_group_members(state.client_id);
            }
            state
                .pending_media_receive_keys
                .push((user_id, demux_id, ratchet_counter, secret));
        }
    }

    fn send_media_send_key_to_user_over_signaling(
        state: &mut State,
        recipient_id: UserId,
        local_demux_id: DemuxId,
        ratchet_counter: frame_crypto::RatchetCounter,
        secret: frame_crypto::Secret,
    ) {
        info!("send_media_send_key_to_user_over_signaling():");
        debug!("  recipient_id: {}", uuid_to_string(&recipient_id));

        let media_key = protobuf::group_call::device_to_device::MediaKey {
            demux_id: Some(local_demux_id),
            ratchet_counter: Some(ratchet_counter as u32),
            secret: Some(secret.to_vec()),
        };
        let message = protobuf::group_call::DeviceToDevice {
            group_id: Some(state.group_id.clone()),
            media_key: Some(media_key),
            ..Default::default()
        };
        let call_message = protobuf::signaling::CallMessage {
            group_call_message: Some(message),
            ..Default::default()
        };

        state.observer.send_signaling_message(
            recipient_id,
            call_message,
            SignalingMessageUrgency::Droppable,
        );
    }

    fn send_pending_media_send_key_to_users_with_added_devices(
        state: &mut State,
        users_with_added_devices: &[UserId],
    ) {
        info!(
            "Sending pending media key to users with added devices (number of users: {}).",
            users_with_added_devices.len()
        );
        if let JoinState::Joined(local_demux_id) = state.join_state {
            if let KeyRotationState::Pending { secret, .. } = state.media_send_key_rotation_state {
                for user_id in users_with_added_devices.iter() {
                    Self::send_media_send_key_to_user_over_signaling(
                        state,
                        user_id.clone(),
                        local_demux_id,
                        0,
                        secret,
                    );
                }
            }
        }
    }

    // The format for the ciphertext is:
    // 1 (audio) or 10 (video) bytes of unencrypted media
    // N bytes of encrypted media (the rest of the given plaintext_size)
    // 1 byte RatchetCounter
    // 4 byte FrameCounter
    // 16 byte MAC
    //
    // Here is the justification for a 4 byte FrameCounter:
    // - With 30fps video with 3 layers:
    //   - an 8min call will require 17 bits
    //   - a 35hr call will require 25 bits
    //   - a 1yr call will require 33 bits
    // - So for most calls we need 3 bytes and for a small number of calls we need 4 bytes.
    // - We could use a varint mechanism to choose between 3 and 4 bytes, but that's not really
    //   worth the extra complexity.
    const FRAME_ENCRYPTION_FOOTER_LEN: usize = size_of::<frame_crypto::RatchetCounter>()
        + size_of::<u32>()
        + size_of::<frame_crypto::Mac>();

    // The portion of the frame we leave in the clear
    // to allow the SFU to forward media properly.
    fn unencrypted_media_header_len(is_audio: bool) -> usize {
        if is_audio {
            // For Opus TOC
            1
        } else {
            // For VP8 headers
            // TODO: Reduce this to 3 when it's not a key frame
            10
        }
    }

    // Called by WebRTC through PeerConnectionObserver
    // See comment on FRAME_ENCRYPTION_FOOTER_LEN for more details on the format
    fn get_ciphertext_buffer_size(plaintext_size: usize) -> usize {
        // If we get asked to encrypt a message of size greater than (usize::MAX - FRAME_ENCRYPTION_FOOTER_LEN),
        // we'd fail to write the footer in encrypt_media and the frame would be dropped.
        plaintext_size.saturating_add(Self::FRAME_ENCRYPTION_FOOTER_LEN)
    }

    // Called by WebRTC through PeerConnectionObserver
    // See comment on FRAME_ENCRYPTION_FOOTER_LEN for more details on the format
    fn encrypt_media(
        &self,
        is_audio: bool,
        plaintext: &[u8],
        ciphertext_buffer: &mut [u8],
    ) -> Result<usize> {
        let mut frame_crypto_context = self
            .frame_crypto_context
            .lock()
            .expect("Get e2ee context to encrypt media");

        let unencrypted_header_len = Self::unencrypted_media_header_len(is_audio);
        Self::encrypt(
            &mut frame_crypto_context,
            unencrypted_header_len,
            plaintext,
            ciphertext_buffer,
        )
    }

    fn encrypt_data(state: &mut State, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut frame_crypto_context = state
            .frame_crypto_context
            .lock()
            .expect("Get e2ee context to encrypt data");

        let mut ciphertext = vec![0; Self::get_ciphertext_buffer_size(plaintext.len())];
        Self::encrypt(&mut frame_crypto_context, 0, plaintext, &mut ciphertext)?;
        Ok(ciphertext)
    }

    fn encrypt(
        frame_crypto_context: &mut frame_crypto::Context,
        unencrypted_header_len: usize,
        plaintext: &[u8],
        ciphertext_buffer: &mut [u8],
    ) -> Result<usize> {
        let ciphertext_size = Self::get_ciphertext_buffer_size(plaintext.len());
        let mut plaintext = Reader::new(plaintext);
        let mut ciphertext = Writer::new(ciphertext_buffer);

        let unencrypted_header = plaintext.read_slice(unencrypted_header_len)?;
        ciphertext.write_slice(unencrypted_header)?;
        let encrypted_payload = ciphertext.write_slice(plaintext.remaining())?;

        let mut mac = frame_crypto::Mac::default();
        let (ratchet_counter, frame_counter) =
            frame_crypto_context.encrypt(encrypted_payload, unencrypted_header, &mut mac)?;
        if frame_counter > u32::MAX as u64 {
            return Err(RingRtcError::FrameCounterTooBig.into());
        }

        ciphertext.write_u8(ratchet_counter)?;
        ciphertext.write_u32(frame_counter as u32)?;
        ciphertext.write_slice(&mac)?;

        Ok(ciphertext_size)
    }

    // Called by WebRTC through PeerConnectionObserver
    // See comment on FRAME_ENCRYPTION_FOOTER_LEN for more details on the format
    fn get_plaintext_buffer_size(ciphertext_size: usize) -> usize {
        // If we get asked to decrypt a message of size less than FRAME_ENCRYPTION_FOOTER_LEN,
        // we'd fail to read the footer in encrypt_media and the frame would be dropped.
        ciphertext_size.saturating_sub(Self::FRAME_ENCRYPTION_FOOTER_LEN)
    }

    // See comment on FRAME_ENCRYPTION_FOOTER_LEN for more details on the format
    fn decrypt_media(
        &self,
        remote_demux_id: DemuxId,
        is_audio: bool,
        ciphertext: &[u8],
        plaintext_buffer: &mut [u8],
    ) -> Result<usize> {
        let mut frame_crypto_context = self
            .frame_crypto_context
            .lock()
            .expect("Get e2ee context to decrypt media");

        let unencrypted_header_len = Self::unencrypted_media_header_len(is_audio);
        Self::decrypt(
            &mut frame_crypto_context,
            remote_demux_id,
            unencrypted_header_len,
            ciphertext,
            plaintext_buffer,
        )
    }

    fn decrypt_data(&self, remote_demux_id: DemuxId, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut frame_crypto_context = self
            .frame_crypto_context
            .lock()
            .expect("Get e2ee context to encrypt data");

        let mut plaintext = vec![0; Self::get_plaintext_buffer_size(ciphertext.len())];
        Self::decrypt(
            &mut frame_crypto_context,
            remote_demux_id,
            0,
            ciphertext,
            &mut plaintext,
        )?;
        Ok(plaintext)
    }

    fn decrypt(
        frame_crypto_context: &mut frame_crypto::Context,
        remote_demux_id: DemuxId,
        unencrypted_header_len: usize,
        ciphertext: &[u8],
        plaintext_buffer: &mut [u8],
    ) -> Result<usize> {
        let mut ciphertext = Reader::new(ciphertext);
        let mut plaintext = Writer::new(plaintext_buffer);

        let unencrypted_header = ciphertext.read_slice(unencrypted_header_len)?;
        let mac: frame_crypto::Mac = ciphertext
            .read_slice_from_end(size_of::<frame_crypto::Mac>())?
            .try_into()?;
        let frame_counter = ciphertext.read_u32_from_end()?;
        let ratchet_counter = ciphertext.read_u8_from_end()?;

        plaintext.write_slice(unencrypted_header)?;
        let encrypted_payload = plaintext.write_slice(ciphertext.remaining())?;

        frame_crypto_context.decrypt(
            remote_demux_id,
            ratchet_counter,
            frame_counter as u64,
            encrypted_payload,
            unencrypted_header,
            &mac,
        )?;
        Ok(unencrypted_header.len() + encrypted_payload.len())
    }

    fn send_heartbeat(state: &mut State) -> Result<()> {
        let heartbeat_msg = protobuf::group_call::DeviceToDevice {
            heartbeat: {
                Some(protobuf::group_call::device_to_device::Heartbeat {
                    audio_muted: state.outgoing_heartbeat_state.audio_muted,
                    video_muted: state.outgoing_heartbeat_state.video_muted,
                    presenting: state.outgoing_heartbeat_state.presenting,
                    sharing_screen: state.outgoing_heartbeat_state.sharing_screen,
                })
            },
            ..Default::default()
        };
        Self::broadcast_data_through_sfu(state, &heartbeat_msg.encode_to_vec())
    }

    fn send_leave_to_sfu(state: &mut State) {
        use protobuf::group_call::{device_to_sfu::LeaveMessage, DeviceToSfu};
        let msg = DeviceToSfu {
            leave: Some(LeaveMessage {}),
            ..Default::default()
        }
        .encode_to_vec();

        if let Err(e) = Self::send_data_to_sfu(state, &msg) {
            warn!("Failed to send LeaveMessage: {:?}", e);
        }
        // Send it *again* to increase reliability just a little.
        if let Err(e) = Self::send_data_to_sfu(state, &msg) {
            warn!("Failed to send extra redundancy LeaveMessage: {:?}", e);
        }
    }

    fn send_leaving_through_sfu_and_over_signaling(state: &mut State, local_demux_id: DemuxId) {
        use protobuf::group_call::{device_to_device::Leaving, DeviceToDevice};

        debug!(
            "group_call::Client(inner)::send_leaving_through_sfu_and_over_signaling(client_id: {}, local_demux_id: {})",
            state.client_id, local_demux_id,
        );

        let msg = DeviceToDevice {
            leaving: Some(Leaving::default()),
            ..DeviceToDevice::default()
        };
        if Self::broadcast_data_through_sfu(state, &msg.encode_to_vec()).is_err() {
            warn!("Could not send leaving message through the SFU");
        } else {
            debug!("Send leaving message over RTP through SFU.");
        }

        let msg = protobuf::signaling::CallMessage {
            group_call_message: Some(DeviceToDevice {
                group_id: Some(state.group_id.clone()),
                leaving: Some(Leaving {
                    demux_id: Some(local_demux_id),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        debug!(
            "Send leaving message to everyone over signaling (recipients: {:?}).",
            state.joined_members
        );
        for user_id in &state.joined_members {
            state.observer.send_signaling_message(
                user_id.clone(),
                msg.clone(),
                SignalingMessageUrgency::Droppable,
            );
        }
    }

    fn cancel_full_group_ring_if_needed(state: &mut State) {
        debug!(
            "group_call::Client(inner)::cancel_full_group_ring_if_needed(client_id: {})",
            state.client_id,
        );

        if let OutgoingRingState::HasSentRing { ring_id } = state.outgoing_ring_state {
            let message = protobuf::signaling::CallMessage {
                ring_intention: Some(protobuf::signaling::call_message::RingIntention {
                    group_id: Some(state.group_id.clone()),
                    ring_id: Some(ring_id.into()),
                    r#type: Some(
                        protobuf::signaling::call_message::ring_intention::Type::Cancelled.into(),
                    ),
                }),
                ..Default::default()
            };

            state.observer.send_signaling_message_to_group(
                state.group_id.clone(),
                message,
                SignalingMessageUrgency::HandleImmediately,
            );
        }
    }

    fn broadcast_data_through_sfu(state: &mut State, message: &[u8]) -> Result<()> {
        debug!(
            "group_call::Client(inner)::broadcast_data_through_sfu(client_id: {}, message: {:?})",
            state.client_id, message,
        );
        if let JoinState::Joined(local_demux_id) = state.join_state {
            let message = Self::encrypt_data(state, message)?;
            let seqnum = state.rtp_data_through_sfu_next_seqnum;
            state.rtp_data_through_sfu_next_seqnum =
                state.rtp_data_through_sfu_next_seqnum.wrapping_add(1);

            let header = rtp::Header {
                pt: RTP_DATA_PAYLOAD_TYPE,
                ssrc: local_demux_id.saturating_add(RTP_DATA_THROUGH_SFU_SSRC_OFFSET),
                // This has to be incremented to make sure SRTP functions properly.
                seqnum: seqnum as u16,
                // Just imagine the clock is the number of heartbeat ticks :).
                // Plus the above sequence number is too small to be useful.
                timestamp: seqnum,
            };
            state.peer_connection.send_rtp(header, &message)?;
        }
        Ok(())
    }

    fn send_data_to_sfu(state: &mut State, message: &[u8]) -> Result<()> {
        debug!(
            "group_call::Client(inner)::send_data_to_sfu(client_id: {}, message: {:?})",
            state.client_id, message,
        );
        if let JoinState::Joined(_) = state.join_state {
            let seqnum = state.rtp_data_to_sfu_next_seqnum;
            state.rtp_data_to_sfu_next_seqnum = state.rtp_data_to_sfu_next_seqnum.wrapping_add(1);

            let header = rtp::Header {
                pt: RTP_DATA_PAYLOAD_TYPE,
                ssrc: RTP_DATA_TO_SFU_SSRC,
                // This has to be incremented to make sure SRTP functions properly.
                seqnum: seqnum as u16,
                // Just imagine the clock is the number of messages :),
                // Plus the above sequence number is too small to be useful.
                timestamp: seqnum,
            };
            state.peer_connection.send_rtp(header, message)?;
        }
        Ok(())
    }

    fn handle_rtp_received(&self, header: rtp::Header, payload: &[u8]) {
        use protobuf::group_call::{
            sfu_to_device::{CurrentDevices, DeviceJoinedOrLeft, Removed, Speaker},
            DeviceToDevice, SfuToDevice,
        };

        if header.pt == RTP_DATA_PAYLOAD_TYPE {
            if header.ssrc == RTP_DATA_TO_SFU_SSRC {
                // TODO: Use video_request to throttle down how much we send when it's not needed.
                if let Ok(SfuToDevice {
                    speaker,
                    device_joined_or_left,
                    current_devices,
                    stats,
                    video_request: _,
                    removed,
                }) = SfuToDevice::decode(payload)
                {
                    if let Some(Speaker {
                        demux_id: speaker_demux_id,
                    }) = speaker
                    {
                        if let Some(speaker_demux_id) = speaker_demux_id {
                            self.handle_speaker_received(header.timestamp, speaker_demux_id);
                        } else {
                            warn!("Ignoring speaker demux ID of None from SFU");
                        }
                    };
                    if let Some(DeviceJoinedOrLeft {}) = device_joined_or_left {
                        self.handle_remote_device_joined_or_left();
                    }
                    // TODO: Use all_demux_ids to avoid polling
                    if let Some(CurrentDevices {
                        demux_ids_with_video,
                        all_demux_ids: _,
                        allocated_heights,
                    }) = current_devices
                    {
                        self.handle_forwarding_video_received(
                            demux_ids_with_video,
                            allocated_heights,
                        );
                    }
                    if let Some(stats) = stats {
                        info!(
                            "ringrtc_stats!,sfu,recv,{},{},{}",
                            stats.target_send_rate_kbps.unwrap_or(0),
                            stats.ideal_send_rate_kbps.unwrap_or(0),
                            stats.allocated_send_rate_kbps.unwrap_or(0)
                        );
                    }
                    if let Some(Removed {}) = removed {
                        self.handle_removed_received();
                    }
                }
                debug!("Received RTP data from SFU: {:?}.", payload);
            } else {
                let demux_id = header.ssrc.saturating_sub(RTP_DATA_THROUGH_SFU_SSRC_OFFSET);
                if let Ok(payload) = self.decrypt_data(demux_id, payload) {
                    if let Ok(msg) = DeviceToDevice::decode(&payload[..]) {
                        if let Some(heartbeat) = msg.heartbeat {
                            self.handle_heartbeat_received(demux_id, header.timestamp, heartbeat);
                        }
                        if let Some(_leaving) = msg.leaving {
                            self.actor.send(move |state| {
                                Self::handle_leaving_received(state, demux_id);
                            });
                        }
                    } else {
                        warn!(
                            "Ignoring received RTP data because decoding failed. demux_id: {}",
                            demux_id,
                        );
                    }
                } else {
                    warn!(
                        "Ignoring received RTP data because decryption failed. demux_id: {}",
                        demux_id,
                    );
                }
                self.actor.send(move |state| {
                    let known = state
                        .remote_devices
                        .iter()
                        .any(|rd| rd.demux_id == demux_id);
                    if !known {
                        // It's likely this demux_id just joined.
                        debug!("Request devices because we just received a heartbeat from unknown demux_id = {}", demux_id);
                        Self::request_remote_devices_as_soon_as_possible(state);
                    }
                });
            }
        } else {
            warn!(
                "Ignoring received RTP data with unknown payload type: {}",
                header.pt
            );
        }
    }

    fn handle_removed_received(&self) {
        self.actor.send(move |state| {
            if state.has_ever_been_participating_client {
                Self::end(state, EndReason::RemovedFromCall);
            } else {
                Self::end(state, EndReason::DeniedRequestToJoinCall);
            }
        });
    }

    fn handle_speaker_received(&self, timestamp: rtp::Timestamp, demux_id: DemuxId) {
        self.actor.send(move |state| {
            if let Some(speaker_rtp_timestamp) = state.speaker_rtp_timestamp {
                if timestamp <= speaker_rtp_timestamp {
                    // Ignored packets received out of order
                    debug!(
                        "Ignoring speaker change because the timestamp is old: {}",
                        timestamp
                    );
                    return;
                }
            }
            state.speaker_rtp_timestamp = Some(timestamp);

            let latest_speaker_demux_id = state.remote_devices.latest_speaker_demux_id();

            if let Some(speaker_device) = state.remote_devices.find_by_demux_id_mut(demux_id) {
                if latest_speaker_demux_id == Some(speaker_device.demux_id) {
                    debug!(
                        "Already the latest speaker demux {:?} since {:?}",
                        speaker_device.demux_id, speaker_device.speaker_time
                    );
                    return;
                }

                speaker_device.speaker_time = Some(SystemTime::now());
                info!(
                    "New speaker {:?} at {:?}",
                    speaker_device.demux_id, speaker_device.speaker_time
                );
                let demux_id = speaker_device.demux_id;
                state.observer.handle_remote_devices_changed(
                    state.client_id,
                    &state.remote_devices,
                    RemoteDevicesChangedReason::SpeakerTimeChanged(demux_id),
                );
            } else {
                debug!(
                    "Ignoring speaker change because it isn't a known remote devices: {}",
                    demux_id
                );
                // Unknown speaker device. It's probably the local device.
            }
        });
    }

    fn handle_remote_device_joined_or_left(&self) {
        self.actor.send(move |state| {
            info!("SFU notified that a remote device has joined or left, requesting update");
            Self::request_remote_devices_as_soon_as_possible(state);
        })
    }

    fn handle_forwarding_video_received(
        &self,
        mut demux_ids_with_video: Vec<DemuxId>,
        allocated_heights: Vec<u32>,
    ) {
        self.actor.send(move |state| {
            let forwarding_videos: HashMap<DemuxId, u16> = demux_ids_with_video
                .iter()
                .zip(allocated_heights.iter())
                .map(|(&demux_id, &height)| (demux_id, height as u16))
                .collect();
            if state.forwarding_videos != forwarding_videos {
                demux_ids_with_video.sort_unstable();
                info!(
                    "SFU notified that the forwarding videos changed. Demux IDs with video is now {:?}",
                    demux_ids_with_video
                );
                for remote_device in state.remote_devices.iter_mut() {
                    let server_allocated_height = forwarding_videos.get(&remote_device.demux_id);
                    let is_forwarding = server_allocated_height.is_some();
                    remote_device.forwarding_video = Some(is_forwarding);
                    remote_device.server_allocated_height = server_allocated_height.copied().unwrap_or(0);

                    if !is_forwarding {
                        remote_device.client_decoded_height = None;
                    }

                    remote_device.recalculate_higher_resolution_pending();
                }
                state.forwarding_videos = forwarding_videos;
                state.observer.handle_remote_devices_changed(
                    state.client_id,
                    &state.remote_devices,
                    RemoteDevicesChangedReason::ForwardedVideosChanged,
                )
            }
        })
    }

    fn handle_heartbeat_received(
        &self,
        demux_id: DemuxId,
        timestamp: u32,
        heartbeat: protobuf::group_call::device_to_device::Heartbeat,
    ) {
        self.actor.send(move |state| {
            if let Some(remote_device) = state.remote_devices.find_by_demux_id_mut(demux_id) {
                if timestamp > remote_device.heartbeat_rtp_timestamp.unwrap_or(0) {
                    // Record this even if nothing changed.  Otherwise an old packet could override
                    // a new packet.
                    remote_device.heartbeat_rtp_timestamp = Some(timestamp);
                    let heartbeat_state = HeartbeatState::from(heartbeat);
                    if remote_device.heartbeat_state != heartbeat_state {
                        if heartbeat_state.video_muted == Some(true) {
                            remote_device.client_decoded_height = None;
                            remote_device.recalculate_higher_resolution_pending();
                        }

                        remote_device.heartbeat_state = heartbeat_state;

                        state.observer.handle_remote_devices_changed(
                            state.client_id,
                            &state.remote_devices,
                            RemoteDevicesChangedReason::HeartbeatStateChanged(demux_id),
                        );
                    }
                }
            } else {
                warn!(
                    "Ignoring received heartbeat for unknown demux_id {}",
                    demux_id
                );
            }
        });
    }

    fn handle_leaving_received(state: &mut State, demux_id: DemuxId) {
        // It's likely we haven't received an update from the SFU about this demux_id leaving.
        debug!(
            "Request devices because we just received a leaving message from demux_id = {}",
            demux_id
        );
        if let Some(device) = state.remote_devices.find_by_demux_id_mut(demux_id) {
            if !device.leaving_received {
                device.leaving_received = true;
                Self::request_remote_devices_as_soon_as_possible(state);

                // It's also possible we have learned this before the SFU has, in which case the SFU may have stale data.
                // So let's wait a little while and ask again.
                state
                    .actor
                    .send_delayed(Duration::from_secs(2), move |state| {
                        info!("Request devices because we received a leaving message from demux_id = {} a while ago", demux_id);
                        Self::request_remote_devices_as_soon_as_possible(state);
                    });
            }
        }
    }

    #[cfg(feature = "sim")]
    pub fn synchronize(&self) {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let barrier_for_task = barrier.clone();

        self.actor.send(move |_| {
            barrier_for_task.wait();
        });

        barrier.wait();
    }
}

// We need to wrap a Call to implement PeerConnectionObserverTrait
// because we need to pass an impl into PeerConnectionObserver::new
// before we call PeerConnectionFactory::create_peer_connection.
// So we need to either have an Option<PeerConnection> inside of the
// State or have an Option<Call> instead of here.  This seemed
// more convenient (fewer "if let Some(x) = x" to do).
struct PeerConnectionObserverImpl {
    client: Option<Client>,
    incoming_video_sink: Option<Box<dyn VideoSink>>,
    last_height_by_track_id: HashMap<u32, u32>,
}

impl PeerConnectionObserverImpl {
    fn uninitialized(
        incoming_video_sink: Option<Box<dyn VideoSink>>,
    ) -> Result<(Box<Self>, PeerConnectionObserver<Self>)> {
        let enable_video_frame_content = incoming_video_sink.is_some();
        let boxed_observer_impl = Box::new(Self {
            client: None,
            incoming_video_sink,
            last_height_by_track_id: HashMap::new(),
        });
        let observer = PeerConnectionObserver::new(
            webrtc::ptr::Borrowed::from_ptr(&*boxed_observer_impl),
            true, /* enable_frame_encryption */
            true, /* enable_video_frame_event */
            enable_video_frame_content,
        )?;
        Ok((boxed_observer_impl, observer))
    }

    fn initialize(&mut self, client: Client) {
        self.client = Some(client);
    }
}

impl PeerConnectionObserverTrait for PeerConnectionObserverImpl {
    fn log_id(&self) -> &dyn std::fmt::Display {
        if let Some(client) = &self.client {
            &client.client_id
        } else {
            &"Call that hasn't been setup yet."
        }
    }

    fn handle_ice_candidate_gathered(
        &mut self,
        _ice_candidate: signaling::IceCandidate,
        _sdp_for_logging: &str,
        _relay_protocol: Option<webrtc::peer_connection_observer::TransportProtocol>,
    ) -> Result<()> {
        Ok(())
    }

    fn handle_ice_candidates_removed(&mut self, _removed_addresses: Vec<SocketAddr>) -> Result<()> {
        Ok(())
    }

    fn handle_ice_connection_state_changed(
        &mut self,
        ice_connection_state: IceConnectionState,
    ) -> Result<()> {
        debug!(
            "group_call::Client(outer)::handle_ice_connection_state_changed(client_id: {}, state: {:?})",
            self.log_id(),
            ice_connection_state
        );
        if let Some(client) = &self.client {
            client.actor.send(move |state| {
                debug!("group_call::Client(inner)::handle_ice_connection_state_changed(client_id: {}, state: {:?})", state.client_id, ice_connection_state);

                match (state.connection_state, ice_connection_state) {
                    (ConnectionState::Connecting, IceConnectionState::Disconnected) |
                    (ConnectionState::Connecting, IceConnectionState::Closed) |
                    (ConnectionState::Connecting, IceConnectionState::Failed) => {
                        // ICE failed before we got connected :(
                        Client::end(state, EndReason::IceFailedWhileConnecting);
                    }
                    (ConnectionState::Connecting, IceConnectionState::Checking) => {
                        // Normal.  Not much to report.
                    }
                    (ConnectionState::Connecting, IceConnectionState::Connected) |
                    (ConnectionState::Connecting, IceConnectionState::Completed) => {
                        // ICE Connected!
                        Client::set_connection_state_and_notify_observer(state, ConnectionState::Connected);
                    }
                    (ConnectionState::Connected, IceConnectionState::Checking) |
                    (ConnectionState::Connected, IceConnectionState::Disconnected) => {
                        // Some connectivity problems, hopefully temporary.
                        Client::set_connection_state_and_notify_observer(state, ConnectionState::Reconnecting);
                    }
                    (ConnectionState::Reconnecting, IceConnectionState::Connected) |
                    (ConnectionState::Reconnecting, IceConnectionState::Completed) => {
                        // The connectivity problems have gone away it seems.
                        Client::set_connection_state_and_notify_observer(state, ConnectionState::Connected);
                    }
                    (_, IceConnectionState::Failed) |
                    (_, IceConnectionState::Closed) => {
                        // The connectivity problems persisted.  ICE has failed.
                        Client::end(state, EndReason::IceFailedAfterConnected);
                    }
                    (_, _) => {
                        warn!("Could not process ICE connection state {:?} while in group call ConnectionState {:?}", ice_connection_state, state.connection_state);
                    }
                }
            });
        } else {
            warn!("Call isn't setup yet!");
        }
        Ok(())
    }

    fn handle_ice_network_route_changed(&mut self, network_route: NetworkRoute) -> Result<()> {
        debug!(
            "group_call::Client(outer)::handle_ice_network_route_changed(client_id: {}, network_route: {:?})",
            self.log_id(),
            network_route
        );
        if let Some(client) = &self.client {
            client.actor.send(move |state| {
                debug!("group_call::Client(inner)::handle_ice_network_route_changed(client_id: {}, network_route: {:?})", state.client_id, network_route);
                state
                    .observer
                    .handle_network_route_changed(state.client_id, network_route);
            });
        } else {
            warn!("Call isn't setup yet!");
        }
        Ok(())
    }

    fn handle_incoming_video_added(&mut self, incoming_video_track: VideoTrack) -> Result<()> {
        debug!(
            "group_call::Client(outer)::handle_incoming_video_track(client_id: {})",
            self.log_id()
        );
        if let Some(client) = &self.client {
            client.actor.send(move |state| {
                debug!(
                    "group_call::Client(inner)::handle_incoming_video_track(client_id: {})",
                    state.client_id
                );

                if let Some(remote_demux_id) = incoming_video_track.id() {
                    // When PeerConnection::SetRemoteDescription triggers PeerConnectionObserver::OnAddTrack,
                    // if it's a VideoTrack, this is where it comes.  Each platform does different things:
                    // - iOS: The VideoTrack is wrapped in an RTCVideoTrack and passed to the app
                    //        via handleIncomingVideoTrack and onRemoteDeviceStatesChanged, which adds a sink.
                    // - Android: The VideoTrack is wrapped in a Java VideoTrack and passed to the app via handleIncomingVideoTrack, which adds a sink.
                    // - Desktop: A VideoSink is added by the PeerConnectionObserverRffi.
                    state.observer.handle_incoming_video_track(
                        state.client_id,
                        remote_demux_id,
                        incoming_video_track,
                    )
                } else {
                    warn!("Ignoring incoming video track with unparsable ID",);
                }
            });
        } else {
            warn!("Call isn't setup yet!");
        }
        Ok(())
    }

    fn handle_incoming_video_frame(
        &mut self,
        track_id: u32,
        video_frame_metadata: VideoFrameMetadata,
        video_frame: Option<VideoFrame>,
    ) -> Result<()> {
        let height = video_frame_metadata.height;
        if let (Some(incoming_video_sink), Some(video_frame)) =
            (self.incoming_video_sink.as_ref(), video_frame)
        {
            incoming_video_sink.on_video_frame(track_id, video_frame)
        }
        if let Some(client) = &self.client {
            let prev_height = self.last_height_by_track_id.insert(track_id, height);
            if prev_height != Some(height) {
                client.actor.send(move |state| {
                    if let Some(remote_device) = state.remote_devices.find_by_demux_id_mut(track_id)
                    {
                        // The height needs to be checked again because last_height_by_track_id
                        // doesn't account for video mute or forwarding state.
                        if remote_device.client_decoded_height != Some(height)
                            // Workaround for a race where a frame is received after video muting
                            && remote_device.heartbeat_state.video_muted != Some(true)
                        {
                            remote_device.client_decoded_height = Some(height);

                            let was_higher_resolution_pending =
                                remote_device.is_higher_resolution_pending;
                            remote_device.recalculate_higher_resolution_pending();

                            if remote_device.is_higher_resolution_pending
                                != was_higher_resolution_pending
                            {
                                state.observer.handle_remote_devices_changed(
                                    state.client_id,
                                    &state.remote_devices,
                                    RemoteDevicesChangedReason::HigherResolutionPendingChanged,
                                );
                            }
                        }
                    }
                });
            }
        }

        Ok(())
    }

    fn handle_rtp_received(&mut self, header: rtp::Header, payload: &[u8]) {
        if let Some(client) = &self.client {
            client.handle_rtp_received(header, payload);
        } else {
            warn!(
                "Ignoring received RTP data with SSRC {} because the call isn't setup",
                header.ssrc
            );
        }
    }

    fn get_media_ciphertext_buffer_size(
        &mut self,
        _is_audio: bool,
        plaintext_size: usize,
    ) -> usize {
        Client::get_ciphertext_buffer_size(plaintext_size)
    }

    // See comment on FRAME_ENCRYPTION_FOOTER_LEN for more details on the format
    fn encrypt_media(
        &mut self,
        is_audio: bool,
        plaintext: &[u8],
        ciphertext_buffer: &mut [u8],
    ) -> Result<usize> {
        if let Some(client) = &self.client {
            client.encrypt_media(is_audio, plaintext, ciphertext_buffer)
        } else {
            warn!("Call isn't setup yet!  Can't encrypt.");
            Err(RingRtcError::FailedToEncrypt.into())
        }
    }

    fn get_media_plaintext_buffer_size(
        &mut self,
        _track_id: u32,
        _is_audio: bool,
        ciphertext_size: usize,
    ) -> usize {
        Client::get_plaintext_buffer_size(ciphertext_size)
    }

    // See comment on FRAME_ENCRYPTION_FOOTER_LEN for more details on the format
    fn decrypt_media(
        &mut self,
        track_id: u32,
        is_audio: bool,
        ciphertext: &[u8],
        plaintext_buffer: &mut [u8],
    ) -> Result<usize> {
        if let Some(client) = &self.client {
            let remote_demux_id = track_id;
            client.decrypt_media(remote_demux_id, is_audio, ciphertext, plaintext_buffer)
        } else {
            warn!("Call isn't setup yet!  Can't decrypt");
            Err(RingRtcError::FailedToDecrypt.into())
        }
    }
}

fn random_alphanumeric(len: usize) -> String {
    std::iter::repeat(())
        .map(|()| rand::rngs::OsRng.sample(rand::distributions::Alphanumeric))
        .take(len)
        .collect()
}

// Should this go in some util class?
struct Writer<'buf> {
    buf: &'buf mut [u8],
    offset: usize,
}

impl<'buf> Writer<'buf> {
    fn new(buf: &'buf mut [u8]) -> Self {
        Self { buf, offset: 0 }
    }

    fn remaining_len(&self) -> usize {
        self.buf.len() - self.offset
    }

    fn write_u8(&mut self, input: u8) -> Result<()> {
        if self.remaining_len() < 1 {
            return Err(RingRtcError::BufferTooSmall.into());
        }
        self.buf[self.offset] = input;
        self.offset += 1;
        Ok(())
    }

    fn write_u32(&mut self, input: u32) -> Result<()> {
        self.write_slice(&input.to_be_bytes())?;
        Ok(())
    }

    fn write_slice(&mut self, input: &[u8]) -> Result<&mut [u8]> {
        if self.remaining_len() < input.len() {
            return Err(RingRtcError::BufferTooSmall.into());
        }
        let start = self.offset;
        let end = start + input.len();
        let output = &mut self.buf[start..end];
        output.copy_from_slice(input);
        self.offset = end;
        Ok(output)
    }
}

struct Reader<'data> {
    data: &'data [u8],
}

impl<'data> Reader<'data> {
    fn new(data: &'data [u8]) -> Self {
        Self { data }
    }

    fn remaining(&self) -> &[u8] {
        self.data
    }

    fn read_u8_from_end(&mut self) -> Result<u8> {
        let (last, rest) = self.data.split_last().ok_or(RingRtcError::BufferTooSmall)?;
        self.data = rest;
        Ok(*last)
    }

    fn read_u32_from_end(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(
            self.read_slice_from_end(size_of::<u32>())?.try_into()?,
        ))
    }

    fn read_slice(&mut self, len: usize) -> Result<&'data [u8]> {
        if len > self.data.len() {
            return Err(RingRtcError::BufferTooSmall.into());
        }
        let (read, rest) = self.data.split_at(len);
        self.data = rest;
        Ok(read)
    }

    fn read_slice_from_end(&mut self, len: usize) -> Result<&'data [u8]> {
        if len > self.data.len() {
            return Err(RingRtcError::BufferTooSmall.into());
        }
        let (rest, read) = self.data.split_at(self.data.len() - len);
        self.data = rest;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{self, AtomicU64},
        mpsc, Arc, Condvar, Mutex,
    };

    use crate::{lite::sfu::PeekDeviceInfo, webrtc::sim::media::FAKE_AUDIO_TRACK};

    use super::*;
    use std::sync::atomic::Ordering;

    #[derive(Clone)]
    struct FakeSfuClient {
        sfu_info: SfuInfo,
        local_demux_id: DemuxId,
        call_creator: Option<UserId>,
        request_count: Arc<AtomicU64>,
        era_id: String,
    }

    impl FakeSfuClient {
        fn new(local_demux_id: DemuxId, call_creator: Option<UserId>) -> Self {
            Self {
                sfu_info: SfuInfo {
                    udp_addresses: Vec::new(),
                    tcp_addresses: Vec::new(),
                    ice_ufrag: "fake ICE ufrag".to_string(),
                    ice_pwd: "fake ICE pwd".to_string(),
                },
                local_demux_id,
                call_creator,
                request_count: Arc::new(AtomicU64::new(0)),
                era_id: "1111111111111111".to_string(),
            }
        }
    }

    impl FakeSfuClient {
        pub fn request_count(&self) -> u64 {
            self.request_count.load(atomic::Ordering::SeqCst)
        }
    }

    impl SfuClient for FakeSfuClient {
        fn join(&mut self, _ice_ufrag: &str, _dhe_pub_key: [u8; 32], client: Client) {
            client.on_sfu_client_joined(Ok(Joined {
                sfu_info: self.sfu_info.clone(),
                local_demux_id: self.local_demux_id,
                server_dhe_pub_key: [0u8; 32],
                hkdf_extra_info: b"hkdf_extra_info".to_vec(),
                creator: self.call_creator.clone(),
                era_id: self.era_id.clone(),
            }));
        }
        fn peek(&mut self, _peek_result_callback: PeekResultCallback) {
            self.request_count.fetch_add(1, atomic::Ordering::SeqCst);
        }
        fn set_group_members(&mut self, _members: Vec<GroupMember>) {}
        fn set_membership_proof(&mut self, _proof: MembershipProof) {}
    }

    // TODO: Put this in common util area?
    #[derive(Clone)]
    struct Waitable<T> {
        val: Arc<Mutex<Option<T>>>,
        cvar: Arc<Condvar>,
    }

    impl<T> Default for Waitable<T> {
        fn default() -> Self {
            Self {
                val: Arc::default(),
                cvar: Arc::default(),
            }
        }
    }

    impl<T: Clone> Waitable<T> {
        fn set(&self, val: T) {
            let mut val_guard = self.val.lock().unwrap();
            *val_guard = Some(val);
            self.cvar.notify_all();
        }

        fn wait(&self, timeout: Duration) -> Option<T> {
            let mut val = self.val.lock().unwrap();
            while val.is_none() {
                let (wait_val, wait_result) = self.cvar.wait_timeout(val, timeout).unwrap();
                if wait_result.timed_out() {
                    return None;
                }
                val = wait_val
            }
            Some(val.take().unwrap())
        }
    }

    #[derive(Clone, Default)]
    struct Event {
        waitable: Waitable<()>,
    }

    impl Event {
        fn set(&self) {
            self.waitable.set(());
        }

        fn wait(&self, timeout: Duration) -> bool {
            self.waitable.wait(timeout).is_some()
        }
    }

    #[derive(Clone, Default)]
    struct FakeObserverPeekState {
        joined_members: Vec<UserId>,
        creator: Option<UserId>,
        era_id: Option<String>,
        max_devices: Option<u32>,
        device_count: usize,
    }

    #[derive(Clone)]
    #[allow(dead_code)] // Ignore clippy warning for era_id due to compile error.
    struct FakeObserver {
        // For sending messages
        user_id: UserId,
        recipients: Arc<CallMutex<Vec<TestClient>>>,
        outgoing_signaling_blocked: Arc<CallMutex<bool>>,
        sent_group_signaling_messages: Arc<CallMutex<Vec<protobuf::signaling::CallMessage>>>,

        connecting: Event,
        joined: Event,
        peek_changed: Event,
        remote_devices_changed: Event,
        remote_devices: Arc<CallMutex<Vec<RemoteDeviceState>>>,
        remote_devices_at_join_time: Arc<CallMutex<Vec<RemoteDeviceState>>>,
        peek_state: Arc<CallMutex<FakeObserverPeekState>>,
        send_rates: Arc<CallMutex<Option<SendRates>>>,
        ended: Waitable<EndReason>,
        era_id: Option<String>,

        request_membership_proof_invocation_count: Arc<AtomicU64>,
        request_group_members_invocation_count: Arc<AtomicU64>,
        handle_remote_devices_changed_invocation_count: Arc<AtomicU64>,
        handle_audio_levels_invocation_count: Arc<AtomicU64>,
    }

    impl FakeObserver {
        fn new(user_id: UserId) -> Self {
            Self {
                user_id,
                recipients: Arc::new(CallMutex::new(Vec::new(), "FakeObserver recipients")),
                outgoing_signaling_blocked: Arc::new(CallMutex::new(
                    false,
                    "FakeObserver outgoing_signaling_blocked",
                )),
                sent_group_signaling_messages: Arc::new(CallMutex::new(
                    Vec::new(),
                    "FakeObserver sent group messages",
                )),
                connecting: Event::default(),
                joined: Event::default(),
                peek_changed: Event::default(),
                remote_devices_changed: Event::default(),
                remote_devices: Arc::new(CallMutex::new(Vec::new(), "FakeObserver remote devices")),
                remote_devices_at_join_time: Arc::new(CallMutex::new(
                    Vec::new(),
                    "FakeObserver remote devices",
                )),
                peek_state: Arc::new(CallMutex::new(
                    FakeObserverPeekState::default(),
                    "FakeObserver peek state",
                )),
                send_rates: Arc::new(CallMutex::new(None, "FakeObserver send rates")),
                ended: Waitable::default(),
                era_id: None,
                request_membership_proof_invocation_count: Default::default(),
                request_group_members_invocation_count: Default::default(),
                handle_remote_devices_changed_invocation_count: Default::default(),
                handle_audio_levels_invocation_count: Default::default(),
            }
        }

        fn set_outgoing_signaling_blocked(&self, blocked: bool) {
            let mut outgoing_signaling_blocked = self
                .outgoing_signaling_blocked
                .lock()
                .expect("Lock outgoing_signaling_blocked to set it");
            *outgoing_signaling_blocked = blocked;
        }

        fn outgoing_signaling_blocked(&self) -> bool {
            let outgoing_signaling_blocked = self
                .outgoing_signaling_blocked
                .lock()
                .expect("Lock outgoing_signaling_blocked to get it");
            *outgoing_signaling_blocked
        }

        fn set_recipients(&self, recipients: Vec<TestClient>) {
            let mut owned_recipients = self
                .recipients
                .lock()
                .expect("Lock recipients to add recipient");
            *owned_recipients = recipients;
        }

        fn remote_devices(&self) -> Vec<RemoteDeviceState> {
            let remote_devices = self
                .remote_devices
                .lock()
                .expect("Lock remote devices to read them");
            remote_devices.iter().cloned().collect()
        }

        fn remote_devices_at_join_time(&self) -> Vec<RemoteDeviceState> {
            let remote_devices_at_join_time = self
                .remote_devices_at_join_time
                .lock()
                .expect("Lock remote devices at join time to read them");
            remote_devices_at_join_time.iter().cloned().collect()
        }

        fn joined_members(&self) -> Vec<UserId> {
            let peek_state = self.peek_state.lock().expect("Lock peek state to read it");
            peek_state.joined_members.to_vec()
        }

        fn peek_state(&self) -> FakeObserverPeekState {
            let peek_state = self.peek_state.lock().expect("Lock peek state to read it");
            peek_state.clone()
        }

        fn send_rates(&self) -> Option<SendRates> {
            let send_rates = self.send_rates.lock().expect("Lock send rates to read it");
            send_rates.clone()
        }

        /// Gets the number of `request_membership_proof` since last checked.
        fn request_membership_proof_invocation_count(&self) -> u64 {
            self.request_membership_proof_invocation_count
                .swap(0, Ordering::Relaxed)
        }

        /// Gets the number of `request_group_members` since last checked.
        fn request_group_members_invocation_count(&self) -> u64 {
            self.request_group_members_invocation_count
                .swap(0, Ordering::Relaxed)
        }

        /// Gets the number of `handle_remote_devices_changed` since last checked.
        fn handle_remote_devices_changed_invocation_count(&self) -> u64 {
            self.handle_remote_devices_changed_invocation_count
                .swap(0, Ordering::Relaxed)
        }

        /// Gets the number of `handle_audio_levels` since last checked.
        fn handle_audio_levels_invocation_count(&self) -> u64 {
            self.handle_audio_levels_invocation_count
                .swap(0, Ordering::Relaxed)
        }
    }

    impl Observer for FakeObserver {
        fn request_membership_proof(&self, _client_id: ClientId) {
            self.request_membership_proof_invocation_count
                .fetch_add(1, Ordering::Relaxed);
        }

        fn request_group_members(&self, _client_id: ClientId) {
            self.request_group_members_invocation_count
                .fetch_add(1, Ordering::Relaxed);
        }

        fn handle_connection_state_changed(
            &self,
            _client_id: ClientId,
            connection_state: ConnectionState,
        ) {
            if connection_state == ConnectionState::Connecting {
                self.connecting.set();
            }
        }

        fn handle_join_state_changed(&self, _client_id: ClientId, join_state: JoinState) {
            if let JoinState::Joined(_) = join_state {
                let mut owned_remote_devices_at_join_time = self
                    .remote_devices_at_join_time
                    .lock()
                    .expect("Lock joined members at join time to handle update");
                *owned_remote_devices_at_join_time = self.remote_devices();
                self.joined.set();
            }
        }

        fn handle_network_route_changed(&self, _client_id: ClientId, _network_route: NetworkRoute) {
        }

        fn handle_remote_devices_changed(
            &self,
            _client_id: ClientId,
            remote_devices: &[RemoteDeviceState],
            _reason: RemoteDevicesChangedReason,
        ) {
            let mut owned_remote_devices = self
                .remote_devices
                .lock()
                .expect("Lock recipients to set remote devices");
            *owned_remote_devices = remote_devices.to_vec();
            self.handle_remote_devices_changed_invocation_count
                .fetch_add(1, Ordering::Relaxed);
            self.remote_devices_changed.set();
        }

        fn handle_audio_levels(
            &self,
            _client_id: ClientId,
            _captured_level: AudioLevel,
            _received_levels: Vec<ReceivedAudioLevel>,
        ) {
            self.handle_audio_levels_invocation_count
                .fetch_add(1, Ordering::Relaxed);
        }

        fn handle_peek_changed(
            &self,
            _client_id: ClientId,
            peek_info: &PeekInfo,
            joined_members: &HashSet<UserId>,
        ) {
            let mut owned_state = self
                .peek_state
                .lock()
                .expect("Lock peek state to handle update");
            owned_state.joined_members = joined_members.iter().cloned().collect();
            owned_state.creator = peek_info.creator.clone();
            owned_state.era_id = peek_info.era_id.clone();
            owned_state.max_devices = peek_info.max_devices;
            owned_state.device_count = peek_info.device_count();
            self.peek_changed.set();
        }

        fn handle_send_rates_changed(&self, _client_id: ClientId, send_rates: SendRates) {
            let mut self_send_rates = self
                .send_rates
                .lock()
                .expect("Lock send rates to handle update");
            *self_send_rates = Some(send_rates);
        }

        fn send_signaling_message(
            &mut self,
            recipient_id: UserId,
            call_message: protobuf::signaling::CallMessage,
            _urgency: SignalingMessageUrgency,
        ) {
            if self.outgoing_signaling_blocked() {
                info!(
                    "Dropping message from {:?} to {:?} because we blocked signaling.",
                    self.user_id, recipient_id
                );
                return;
            }
            let recipients = self
                .recipients
                .lock()
                .expect("Lock recipients to add recipient");
            let mut sent = false;
            if let Some(message) = call_message.group_call_message {
                for recipient in recipients.iter() {
                    if recipient.user_id == recipient_id {
                        recipient
                            .client
                            .on_signaling_message_received(self.user_id.clone(), message.clone());
                        sent = true;
                    }
                }
            }
            if sent {
                info!(
                    "Sent message from {:?} to {:?}.",
                    self.user_id, recipient_id
                );
            } else {
                info!(
                    "Did not sent message from {:?} to {:?} because it's not a known recipient.",
                    self.user_id, recipient_id
                );
            }
        }
        fn send_signaling_message_to_group(
            &mut self,
            _group: GroupId,
            call_message: protobuf::signaling::CallMessage,
            _urgency: SignalingMessageUrgency,
        ) {
            if self.outgoing_signaling_blocked() {
                info!(
                    "Dropping message from {:?} to group because we blocked signaling.",
                    self.user_id,
                );
                return;
            }
            self.sent_group_signaling_messages
                .lock()
                .expect("adding message")
                .push(call_message);
            info!("Recorded group-wide call message from {:?}", self.user_id);
        }
        fn handle_incoming_video_track(
            &mut self,
            _client_id: ClientId,
            _remote_demux_id: DemuxId,
            _incoming_video_track: VideoTrack,
        ) {
        }
        fn handle_ended(&self, _client_id: ClientId, reason: EndReason) {
            self.ended.set(reason);
        }
    }

    #[derive(Clone)]
    struct TestClient {
        user_id: UserId,
        demux_id: DemuxId,
        sfu_client: FakeSfuClient,
        observer: FakeObserver,
        client: Client,
        sfu_rtp_packet_sender: Option<mpsc::Sender<(rtp::Header, Vec<u8>)>>,
        default_peek_info: PeekInfo,
    }

    impl TestClient {
        fn new(user_id: UserId, demux_id: DemuxId) -> Self {
            Self::with_sfu_client(user_id, demux_id, FakeSfuClient::new(demux_id, None))
        }

        fn with_sfu_client(user_id: UserId, demux_id: DemuxId, sfu_client: FakeSfuClient) -> Self {
            let observer = FakeObserver::new(user_id.clone());
            let fake_busy = Arc::new(CallMutex::new(false, "fake_busy"));
            let fake_self_uuid = Arc::new(CallMutex::new(Some(user_id.clone()), "fake_self_uuid"));
            let fake_audio_track = AudioTrack::new(
                webrtc::Arc::from_owned(unsafe {
                    webrtc::ptr::OwnedRc::from_ptr(&FAKE_AUDIO_TRACK as *const u32)
                }),
                None,
            );
            let client = Client::start(
                b"fake group ID".to_vec(),
                demux_id,
                GroupCallKind::SignalGroup,
                Box::new(sfu_client.clone()),
                Box::new(observer.clone()),
                fake_busy,
                fake_self_uuid,
                None,
                fake_audio_track,
                None,
                None,
                None,
                Some(Duration::from_millis(200)),
            )
            .expect("Start Client");
            Self {
                user_id,
                demux_id,
                sfu_client,
                observer,
                client,
                sfu_rtp_packet_sender: None,
                default_peek_info: PeekInfo::default(),
            }
        }

        fn connect_join_and_wait_until_joined(&self) {
            self.client.connect();
            self.client.join();
            assert!(self.observer.joined.wait(Duration::from_secs(5)));
        }

        fn set_up_rtp_with_remotes(&self, clients: Vec<TestClient>) {
            let local_demux_id = self.demux_id;
            let sfu_rtp_packet_sender = self.sfu_rtp_packet_sender.clone();
            self.client.actor.send(move |state| {
                state
                    .peer_connection
                    .set_rtp_packet_sink(Box::new(move |header, payload| {
                        debug!(
                            "Test is going to deliver RTP packet with {:?} and {:?}",
                            header, payload
                        );
                        if header.ssrc == 1 {
                            if let Some(sender) = &sfu_rtp_packet_sender {
                                sender
                                    .send((header, payload.to_vec()))
                                    .expect("Send RTP packet to SFU");
                            }
                        } else {
                            for client in &clients {
                                if client.demux_id != local_demux_id {
                                    client.client.handle_rtp_received(header.clone(), payload)
                                }
                            }
                        }
                    }));
            });
        }

        fn set_remotes_and_wait_until_applied(&self, clients: &[&TestClient]) {
            let remote_devices = clients
                .iter()
                .map(|client| PeekDeviceInfo {
                    demux_id: client.demux_id,
                    user_id: Some(client.user_id.clone()),
                })
                .collect();
            // Need to clone to pass over to the actor and set in observer.
            let clients: Vec<TestClient> = clients.iter().copied().cloned().collect();
            self.observer.set_recipients(clients.clone());
            let peek_info = PeekInfo {
                devices: remote_devices,
                ..self.default_peek_info.clone()
            };
            self.client.set_peek_result(Ok(peek_info));
            self.set_up_rtp_with_remotes(clients);
            self.wait_for_client_to_process();
        }

        fn set_pending_clients_and_wait_until_applied(&self, clients: &[&TestClient]) {
            let remote_devices = clients
                .iter()
                .map(|client| PeekDeviceInfo {
                    demux_id: client.demux_id,
                    user_id: Some(client.user_id.clone()),
                })
                .collect();
            let peek_info = PeekInfo {
                pending_devices: remote_devices,
                ..self.default_peek_info.clone()
            };
            self.client.set_peek_result(Ok(peek_info));
            self.set_up_rtp_with_remotes(vec![]);
            self.wait_for_client_to_process();
        }

        fn wait_for_client_to_process(&self) {
            let event = Event::default();
            let cloned = event.clone();
            self.client.actor.send(move |_state| {
                cloned.set();
            });
            event.wait(Duration::from_secs(5));
        }

        fn encrypt_media(&mut self, is_audio: bool, plaintext: &[u8]) -> Result<Vec<u8>> {
            let mut ciphertext = vec![0; plaintext.len() + Client::FRAME_ENCRYPTION_FOOTER_LEN];
            assert_eq!(
                ciphertext.len(),
                Client::get_ciphertext_buffer_size(plaintext.len())
            );
            assert_eq!(
                ciphertext.len(),
                self.client
                    .encrypt_media(is_audio, plaintext, &mut ciphertext)?
            );
            Ok(ciphertext)
        }

        fn decrypt_media(
            &mut self,
            remote_demux_id: DemuxId,
            is_audio: bool,
            ciphertext: &[u8],
        ) -> Result<Vec<u8>> {
            let mut plaintext = vec![
                0;
                ciphertext
                    .len()
                    .saturating_sub(Client::FRAME_ENCRYPTION_FOOTER_LEN)
            ];
            assert_eq!(
                plaintext.len(),
                Client::get_plaintext_buffer_size(ciphertext.len())
            );
            assert_eq!(
                plaintext.len(),
                self.client
                    .decrypt_media(remote_demux_id, is_audio, ciphertext, &mut plaintext)?
            );
            Ok(plaintext)
        }

        fn receive_speaker(&self, timestamp: u32, speaker_demux_id: DemuxId) {
            self.client
                .handle_speaker_received(timestamp, speaker_demux_id);
            self.wait_for_client_to_process();
        }

        // DemuxIds sorted by speaker_time, then added_time, then demux_id.
        fn speakers(&self) -> Vec<DemuxId> {
            let mut devices = self.observer.remote_devices();
            devices.sort_by_key(|device| {
                (
                    std::cmp::Reverse(device.speaker_time_as_unix_millis()),
                    device.added_time_as_unix_millis(),
                    device.demux_id,
                )
            });
            devices.iter().map(|device| device.demux_id).collect()
        }

        fn disconnect_and_wait_until_ended(&self) {
            self.client.disconnect();
            self.observer.ended.wait(Duration::from_secs(5));
        }
    }

    #[allow(dead_code)]
    fn init_logging() {
        env_logger::builder()
            .is_test(true)
            .filter(None, log::LevelFilter::Debug)
            .init();
    }

    fn set_group_and_wait_until_applied(clients: &[&TestClient]) {
        for client in clients {
            // We're going to be lazy and not remove ourselves.  It shouldn't matter.
            client.set_remotes_and_wait_until_applied(clients);
        }
        for client in clients {
            client.wait_for_client_to_process();
        }
    }

    #[test]
    fn frame_encryption_normal() {
        let mut client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();

        let mut client2 = TestClient::new(vec![2], 2);
        client2.connect_join_and_wait_until_joined();

        client2.set_remotes_and_wait_until_applied(&[&client1]);

        // At this point, client2 knows about client1, so can receive encrypted media.
        // But client1 does not know about client1, so has not yet shared its encryption key
        // with it, so client2 cannot decrypt media from client1.
        // And while client2 has shared the key with client1, client1 has not yet learned
        // about client2 so can't decrypt either.

        let is_audio = true;
        let plaintext = &b"Fake Audio"[..];
        let ciphertext1 = client1.encrypt_media(is_audio, plaintext).unwrap();
        let ciphertext2 = client2.encrypt_media(is_audio, plaintext).unwrap();

        // Check that the first byte for audio is left unencrypted
        // and the rest has changed
        assert_eq!(plaintext[0], ciphertext1[0]);
        assert_ne!(plaintext, &ciphertext1[..plaintext.len()]);

        assert!(client1
            .decrypt_media(client2.demux_id, is_audio, &ciphertext2)
            .is_err());
        assert!(client2
            .decrypt_media(client1.demux_id, is_audio, &ciphertext1)
            .is_err());

        client1.set_remotes_and_wait_until_applied(&[&client2]);
        // We wait until client2 has processed the key from client1
        client2.wait_for_client_to_process();

        // At this point, both clients know about each other and have shared keys
        // and should be able to decrypt.

        // Because client1 just learned about client2, it advanced its key
        // and so we need to re-encrypt with that key.
        let mut ciphertext1 = client1.encrypt_media(is_audio, plaintext).unwrap();

        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext1)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client1
                .decrypt_media(client2.demux_id, is_audio, &ciphertext2)
                .unwrap()
        );

        // But if the footer is too small, decryption should fail
        assert!(client1
            .decrypt_media(client2.demux_id, is_audio, b"small")
            .is_err());

        // And if the unencrypted media header has been modified, it should fail (bad mac)
        ciphertext1[0] = ciphertext1[0].wrapping_add(1);
        assert!(client2
            .decrypt_media(client1.demux_id, is_audio, &ciphertext1)
            .is_err());

        // Finally, let's make sure video works as well

        let is_audio = false;
        let plaintext = &b"Fake Video Needs To Be Bigger"[..];
        let ciphertext1 = client1.encrypt_media(is_audio, plaintext).unwrap();

        // Check that the first 10 bytes of video is left unencrypted
        // and the rest has changed
        assert_eq!(plaintext[..10], ciphertext1[..10]);
        assert_ne!(plaintext, &ciphertext1[..plaintext.len()]);

        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext1)
                .unwrap()
        );

        client1.disconnect_and_wait_until_ended();
        client2.disconnect_and_wait_until_ended();
    }

    #[test]
    #[ignore] // Because it's too slow
    fn frame_encryption_rotation_is_delayed() {
        let mut client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();

        let mut client2 = TestClient::new(vec![2], 2);
        client2.connect_join_and_wait_until_joined();

        let mut client3 = TestClient::new(vec![3], 3);
        client3.connect_join_and_wait_until_joined();

        let mut client4 = TestClient::new(vec![4], 4);
        client4.connect_join_and_wait_until_joined();

        let mut client5 = TestClient::new(vec![5], 5);
        client5.connect_join_and_wait_until_joined();

        set_group_and_wait_until_applied(&[&client1, &client2, &client3]);

        // client2 and client3 can decrypt client1
        // client4 can't yet
        let is_audio = true;
        let plaintext = &b"Fake Audio"[..];
        let ciphertext = client1.encrypt_media(is_audio, plaintext).unwrap();
        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client3
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert!(client4
            .decrypt_media(client1.demux_id, is_audio, &ciphertext)
            .is_err());

        // Add client4 and remove client3
        set_group_and_wait_until_applied(&[&client1, &client2, &client4]);

        // client2 and client4 can decrypt client1
        // client3 can as well, at least for a little while
        let ciphertext = client1.encrypt_media(is_audio, plaintext).unwrap();
        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client3
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client4
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );

        // TODO: Make Actors use tokio so we can use fake time
        std::thread::sleep(std::time::Duration::from_millis(2000));

        // client5 joins during the period between when the new key is generated
        // and when it is applied.  client 5 should receive this key and decrypt
        // both before and after the key is applied.
        // meanwhile, client2 leaves, which will cause another rotation after this
        // one.
        set_group_and_wait_until_applied(&[&client1, &client4, &client5]);

        let ciphertext = client1.encrypt_media(is_audio, plaintext).unwrap();
        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client3
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client4
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client5
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );

        std::thread::sleep(std::time::Duration::from_millis(2000));

        // client4 and client5 can still decrypt from client1
        // but client3 no longer can
        let ciphertext = client1.encrypt_media(is_audio, plaintext).unwrap();
        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert!(client3
            .decrypt_media(client1.demux_id, is_audio, &ciphertext)
            .is_err());
        assert_eq!(
            plaintext,
            client4
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client5
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );

        std::thread::sleep(std::time::Duration::from_millis(3000));

        // After the next key rotation is applied, now client2 cannot decrypt,
        // but client4 and client5 can.
        let ciphertext = client1.encrypt_media(is_audio, plaintext).unwrap();
        assert!(client2
            .decrypt_media(client1.demux_id, is_audio, &ciphertext)
            .is_err());
        assert!(client3
            .decrypt_media(client1.demux_id, is_audio, &ciphertext)
            .is_err());
        assert_eq!(
            plaintext,
            client4
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
        assert_eq!(
            plaintext,
            client5
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );

        client1.disconnect_and_wait_until_ended();
        client2.disconnect_and_wait_until_ended();
        client3.disconnect_and_wait_until_ended();
        client4.disconnect_and_wait_until_ended();
        client5.disconnect_and_wait_until_ended();
    }

    #[test]
    fn frame_encryption_resend_keys() {
        let mut client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();

        let mut client2 = TestClient::new(vec![2], 2);
        client2.connect_join_and_wait_until_joined();

        // Prevent client1 from sharing keys with client2
        client1.observer.set_outgoing_signaling_blocked(true);
        set_group_and_wait_until_applied(&[&client1, &client2]);

        let remote_devices = client2.observer.remote_devices();
        assert_eq!(1, remote_devices.len());
        assert!(!remote_devices[0].media_keys_received);

        let is_audio = false;
        let plaintext = &b"Fake Video is big"[..];
        let ciphertext = client1.encrypt_media(is_audio, plaintext).unwrap();
        // We can't decrypt because the keys got dropped
        assert!(client2
            .decrypt_media(client1.demux_id, is_audio, &ciphertext)
            .is_err());

        client1.observer.set_outgoing_signaling_blocked(false);
        client1.client.resend_media_keys();
        client1.wait_for_client_to_process();
        client2.wait_for_client_to_process();

        let remote_devices = client2.observer.remote_devices();
        assert_eq!(1, remote_devices.len());
        assert!(remote_devices[0].media_keys_received);

        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext)
                .unwrap()
        );
    }

    #[test]
    fn frame_encryption_send_advanced_key_to_same_user() {
        let mut client1a = TestClient::new(vec![1], 11);
        let mut client2a = TestClient::new(vec![2], 21);
        let mut client2b = TestClient::new(vec![2], 22);

        client1a.connect_join_and_wait_until_joined();
        client2a.connect_join_and_wait_until_joined();
        set_group_and_wait_until_applied(&[&client1a, &client2a]);

        let is_audio = true;
        let plaintext = &b"Fake Audio"[..];
        let ciphertext1a = client1a.encrypt_media(is_audio, plaintext).unwrap();
        assert_eq!(
            plaintext,
            client2a
                .decrypt_media(client1a.demux_id, is_audio, &ciphertext1a)
                .unwrap()
        );

        // Make sure the advanced key gets sent to client2b even though it's the same user as 2a.
        client2b.connect_join_and_wait_until_joined();
        set_group_and_wait_until_applied(&[&client1a, &client2a, &client2b]);
        let ciphertext1a = client1a.encrypt_media(is_audio, plaintext).unwrap();
        assert_eq!(
            plaintext,
            client2b
                .decrypt_media(client1a.demux_id, is_audio, &ciphertext1a)
                .unwrap()
        );
    }

    #[test]
    fn frame_encryption_someone_forging_demux_id() {
        let mut client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();

        let mut client2 = TestClient::new(vec![2], 2);
        client2.connect_join_and_wait_until_joined();

        // Client3 is pretending to have demux ID 1 when sending media keys
        let mut client3 = TestClient::with_sfu_client(vec![3], 3, FakeSfuClient::new(1, None));
        client3.connect_join_and_wait_until_joined();

        set_group_and_wait_until_applied(&[&client1, &client2, &client3]);

        let is_audio = true;
        let plaintext = &b"Fake Audio"[..];
        let ciphertext1 = client1.encrypt_media(is_audio, plaintext).unwrap();
        let ciphertext3 = client3.encrypt_media(is_audio, plaintext).unwrap();
        // The forger doesn't mess anything up for the others
        assert_eq!(
            plaintext,
            client2
                .decrypt_media(client1.demux_id, is_audio, &ciphertext1)
                .unwrap()
        );
        // And you can't decrypt from the forger.
        assert!(client2
            .decrypt_media(client3.demux_id, is_audio, &ciphertext3)
            .is_err());

        client1.disconnect_and_wait_until_ended();
        client2.disconnect_and_wait_until_ended();
        client3.disconnect_and_wait_until_ended();
    }

    #[test]
    fn ask_for_group_membership_when_receiving_unknown_media_keys() {
        let client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();
        assert_eq!(1, client1.observer.request_group_members_invocation_count());

        let client2 = TestClient::new(vec![2], 2);
        client2.connect_join_and_wait_until_joined();

        let client3 = TestClient::new(vec![3], 3);
        client3.connect_join_and_wait_until_joined();

        assert_eq!(0, client1.observer.request_group_members_invocation_count());

        // Request group membership for the first unknown media key...
        client2.set_remotes_and_wait_until_applied(&[&client1]);
        client1.wait_for_client_to_process();
        assert_eq!(1, client1.observer.request_group_members_invocation_count());

        // ...but not any after that.
        client3.set_remotes_and_wait_until_applied(&[&client1]);
        client1.wait_for_client_to_process();
        assert_eq!(0, client1.observer.request_group_members_invocation_count());

        // Re-process (and maybe re-request) when the list of active devices changes.
        client1.set_remotes_and_wait_until_applied(&[]);
        assert_eq!(1, client1.observer.request_group_members_invocation_count());

        // Resolving one member results in a re-request, just in case.
        client1.set_remotes_and_wait_until_applied(&[&client2]);
        assert_eq!(1, client1.observer.request_group_members_invocation_count());

        // But resolving the other member is enough to clear the saved list,
        // showing that we already processed the first.
        client1.set_remotes_and_wait_until_applied(&[&client3]);
        assert_eq!(0, client1.observer.request_group_members_invocation_count());
    }

    #[test]
    fn do_not_ask_for_group_membership_when_receiving_known_media_keys() {
        let client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();
        assert_eq!(1, client1.observer.request_group_members_invocation_count());

        let client2 = TestClient::new(vec![2], 2);
        client2.connect_join_and_wait_until_joined();

        assert_eq!(0, client1.observer.request_group_members_invocation_count());

        // This time, the receiver finds out about the sender first...
        client1.set_remotes_and_wait_until_applied(&[&client2]);

        // ...so the media key sent here won't be unknown.
        client2.set_remotes_and_wait_until_applied(&[&client1]);
        client1.wait_for_client_to_process();
        assert_eq!(0, client1.observer.request_group_members_invocation_count());
    }

    #[test]
    fn remote_heartbeat_state() {
        let client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();

        let client2 = TestClient::new(vec![2], 2);
        client2.connect_join_and_wait_until_joined();

        set_group_and_wait_until_applied(&[&client1, &client2]);

        let remote_devices2 = client2.observer.remote_devices();
        assert_eq!(1, remote_devices2.len());
        assert_eq!(client1.demux_id, remote_devices2[0].demux_id);
        assert_eq!(None, remote_devices2[0].heartbeat_state.audio_muted);
        assert_eq!(None, remote_devices2[0].heartbeat_state.video_muted);
        assert_eq!(None, remote_devices2[0].heartbeat_state.presenting);
        assert_eq!(None, remote_devices2[0].heartbeat_state.sharing_screen);

        client1.client.set_outgoing_audio_muted(true);
        client1.wait_for_client_to_process();
        client2.wait_for_client_to_process();

        let remote_devices2 = client2.observer.remote_devices();
        assert_eq!(1, remote_devices2.len());
        assert_eq!(client1.demux_id, remote_devices2[0].demux_id);
        assert_eq!(Some(true), remote_devices2[0].heartbeat_state.audio_muted);
        assert_eq!(None, remote_devices2[0].heartbeat_state.video_muted);
        assert_eq!(None, remote_devices2[0].heartbeat_state.presenting);
        assert_eq!(None, remote_devices2[0].heartbeat_state.sharing_screen);

        client1.client.set_outgoing_video_muted(false);
        client1.wait_for_client_to_process();
        client2.wait_for_client_to_process();

        let remote_devices2 = client2.observer.remote_devices();
        assert_eq!(1, remote_devices2.len());
        assert_eq!(client1.demux_id, remote_devices2[0].demux_id);
        assert_eq!(Some(true), remote_devices2[0].heartbeat_state.audio_muted);
        assert_eq!(Some(false), remote_devices2[0].heartbeat_state.video_muted);
        assert_eq!(None, remote_devices2[0].heartbeat_state.presenting);
        assert_eq!(None, remote_devices2[0].heartbeat_state.sharing_screen);

        client1.client.set_presenting(true);
        client1.wait_for_client_to_process();
        client2.wait_for_client_to_process();

        let remote_devices2 = client2.observer.remote_devices();
        assert_eq!(1, remote_devices2.len());
        assert_eq!(client1.demux_id, remote_devices2[0].demux_id);
        assert_eq!(Some(true), remote_devices2[0].heartbeat_state.audio_muted);
        assert_eq!(Some(false), remote_devices2[0].heartbeat_state.video_muted);
        assert_eq!(Some(true), remote_devices2[0].heartbeat_state.presenting);
        assert_eq!(None, remote_devices2[0].heartbeat_state.sharing_screen);

        client1.client.set_sharing_screen(true);
        client1.wait_for_client_to_process();
        client2.wait_for_client_to_process();

        let remote_devices2 = client2.observer.remote_devices();
        assert_eq!(1, remote_devices2.len());
        assert_eq!(client1.demux_id, remote_devices2[0].demux_id);
        assert_eq!(Some(true), remote_devices2[0].heartbeat_state.audio_muted);
        assert_eq!(Some(false), remote_devices2[0].heartbeat_state.video_muted);
        assert_eq!(Some(true), remote_devices2[0].heartbeat_state.presenting);
        assert_eq!(
            Some(true),
            remote_devices2[0].heartbeat_state.sharing_screen
        );
    }

    fn hash_set<T: std::hash::Hash + Eq + Clone>(vals: impl IntoIterator<Item = T>) -> HashSet<T> {
        vals.into_iter().collect()
    }

    #[test]
    fn ignore_devices_that_arent_members() {
        let client = TestClient::new(vec![1], 1);
        client.connect_join_and_wait_until_joined();

        assert!(client.observer.remote_devices().is_empty());

        let peek_info = PeekInfo {
            devices: vec![
                PeekDeviceInfo {
                    demux_id: 2,
                    user_id: Some(b"2".to_vec()),
                },
                PeekDeviceInfo {
                    demux_id: 3,
                    user_id: None,
                },
            ],
            pending_devices: vec![],
            creator: None,
            era_id: None,
            max_devices: None,
        };
        client.client.set_peek_result(Ok(peek_info));
        client.wait_for_client_to_process();

        let remote_devices = client.observer.remote_devices();
        assert_eq!(1, remote_devices.len());
        assert_eq!(2, remote_devices[0].demux_id);

        assert_eq!(vec![b"2".to_vec()], client.observer.joined_members());
    }

    #[test]
    fn fire_events_on_first_peek_info() {
        let client = TestClient::new(vec![1], 1);

        client.client.connect();
        client.client.set_peek_result(Ok(PeekInfo::default()));

        assert!(client.observer.peek_changed.wait(Duration::from_secs(5)));

        client.client.join();
        client.client.set_peek_result(Ok(PeekInfo {
            // This gets filtered out.  Make sure we still fire the event.
            devices: vec![PeekDeviceInfo {
                demux_id: 1,
                user_id: Some(b"1".to_vec()),
            }],
            pending_devices: vec![],
            creator: None,
            era_id: None,
            max_devices: None,
        }));

        assert!(client
            .observer
            .remote_devices_changed
            .wait(Duration::from_secs(5)));

        assert_eq!(1, client.observer.peek_state().device_count);
    }

    #[test]
    fn joined_members() {
        // The peeker doesn't join
        let peeker = TestClient::new(vec![42], 42);
        peeker.client.connect();
        peeker.wait_for_client_to_process();

        assert_eq!(0, peeker.observer.joined_members().len());

        let joiner1 = TestClient::new(vec![1], 1);
        let joiner2 = TestClient::new(vec![2], 2);

        // The peeker sees updates to the joined members before joining
        peeker.set_remotes_and_wait_until_applied(&[&joiner1]);
        assert_eq!(
            vec![joiner1.user_id.clone()],
            peeker.observer.joined_members()
        );

        peeker.set_remotes_and_wait_until_applied(&[&joiner2]);
        assert_eq!(
            vec![joiner2.user_id.clone()],
            peeker.observer.joined_members()
        );

        peeker.set_remotes_and_wait_until_applied(&[&joiner1, &joiner2]);
        assert_eq!(
            hash_set(&[joiner1.user_id.clone(), joiner2.user_id.clone()]),
            hash_set(&peeker.observer.joined_members())
        );

        // Temporary clear the observer state so we can verify we don't get a
        // callback when nothing changes.
        peeker.observer.handle_peek_changed(
            0,
            &PeekInfo {
                pending_devices: vec![],
                creator: None,
                era_id: None,
                devices: vec![],
                max_devices: None,
            },
            &HashSet::default(),
        );
        assert_eq!(0, peeker.observer.joined_members().len());
        peeker.set_remotes_and_wait_until_applied(&[&joiner1, &joiner2]);
        assert_eq!(0, peeker.observer.joined_members().len());
        peeker.observer.handle_peek_changed(
            0,
            &PeekInfo {
                pending_devices: vec![],
                creator: None,
                era_id: None,
                devices: vec![],
                max_devices: None,
            },
            &([joiner1.user_id.clone(), joiner2.user_id.clone()]
                .iter()
                .cloned()
                .collect()),
        );

        peeker.set_remotes_and_wait_until_applied(&[]);
        assert_eq!(0, peeker.observer.joined_members().len());

        // And the peeker sees updates to the joined members before joining
        peeker.connect_join_and_wait_until_joined();

        peeker.set_remotes_and_wait_until_applied(&[&joiner2]);
        assert_eq!(
            vec![joiner2.user_id.clone()],
            peeker.observer.joined_members()
        );

        peeker.set_remotes_and_wait_until_applied(&[&joiner1, &joiner2]);
        assert_eq!(
            hash_set(&[joiner1.user_id, joiner2.user_id]),
            hash_set(&peeker.observer.joined_members())
        );

        peeker.set_remotes_and_wait_until_applied(&[]);
        assert_eq!(0, peeker.observer.joined_members().len());

        peeker.disconnect_and_wait_until_ended();
    }

    #[test]
    fn pending_clients() {
        let peeker = TestClient::new(vec![42], 42);
        peeker.connect_join_and_wait_until_joined();

        assert_eq!(0, peeker.observer.joined_members().len());

        let joiner1 = TestClient::new(vec![1], 1);
        let joiner2 = TestClient::new(vec![2], 2);

        peeker.set_pending_clients_and_wait_until_applied(&[&joiner1]);
        assert!(peeker
            .observer
            .peek_changed
            .wait(Duration::from_millis(200)));

        peeker.set_pending_clients_and_wait_until_applied(&[&joiner1, &joiner2]);
        assert!(peeker
            .observer
            .peek_changed
            .wait(Duration::from_millis(200)));

        peeker.set_pending_clients_and_wait_until_applied(&[&joiner2, &joiner1]);
        assert!(!peeker
            .observer
            .peek_changed
            .wait(Duration::from_millis(200)));

        peeker.set_pending_clients_and_wait_until_applied(&[&joiner1]);
        assert!(peeker
            .observer
            .peek_changed
            .wait(Duration::from_millis(200)));

        peeker.disconnect_and_wait_until_ended();
    }

    #[test]
    #[ignore] // Because it's too slow
    fn smart_polling() {
        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);

        assert_eq!(0, client1.sfu_client.request_count());

        // We don't query until we get a membership proof
        client1.client.connect();
        client1.wait_for_client_to_process();
        assert_eq!(0, client1.sfu_client.request_count());

        // Once we get a proof, we query immediately
        client1.client.set_membership_proof(b"proof".to_vec());
        client1.wait_for_client_to_process();

        // And when we join(), but only if it's been a while.
        // since we asked before.
        client1.client.join();
        client1.observer.joined.wait(Duration::from_secs(5));
        assert_eq!(1, client1.sfu_client.request_count());
        client1.client.leave();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        client1.client.join();
        // TODO: figure out a way to wait for a second join instead of sleeping.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(2, client1.sfu_client.request_count());
        client1.set_remotes_and_wait_until_applied(&[]);

        // Client2 learns about client1 and sends client crypto keys,
        // which causes client1 to request again.
        client2.connect_join_and_wait_until_joined();
        client2.set_remotes_and_wait_until_applied(&[&client1]);
        client1.wait_for_client_to_process();
        assert_eq!(3, client1.sfu_client.request_count());
        client1.set_remotes_and_wait_until_applied(&[]);

        // Client2 sends a heartbeat to client1
        // which causes client1 to request again.
        std::thread::sleep(std::time::Duration::from_millis(1000));
        assert_eq!(4, client1.sfu_client.request_count());
        client1.set_remotes_and_wait_until_applied(&[&client2]);

        // Client2 sends a leave message to client1
        // which causes client1 to request again.
        // But the SFU hasn't been update yet.
        client2.disconnect_and_wait_until_ended();
        assert_eq!(5, client1.sfu_client.request_count());
        client1.set_remotes_and_wait_until_applied(&[]);

        // Just in case the SFU was old, we request again around 2 seconds
        // after the leave message.
        std::thread::sleep(std::time::Duration::from_millis(2500));
        assert_eq!(6, client1.sfu_client.request_count());
        client1.set_remotes_and_wait_until_applied(&[]);

        // Make sure getting an updated membership proof doesn't mess anything up
        client1.client.set_membership_proof(b"proof".to_vec());
        std::thread::sleep(std::time::Duration::from_millis(5000));
        assert_eq!(6, client1.sfu_client.request_count());

        // And again after around 10 more seconds (infrequent polling).
        std::thread::sleep(std::time::Duration::from_millis(6000));
        assert_eq!(7, client1.sfu_client.request_count());
        client1.set_remotes_and_wait_until_applied(&[]);

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    #[ignore]
    fn polling_error_handling() {
        init_logging();
        let client = TestClient::new(vec![1], 1);
        client.client.set_membership_proof(b"proof".to_vec());
        client.connect_join_and_wait_until_joined();

        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(1, client.sfu_client.request_count());

        std::thread::sleep(std::time::Duration::from_millis(1000));
        assert_eq!(1, client.sfu_client.request_count());

        std::thread::sleep(std::time::Duration::from_millis(1000));
        assert_eq!(1, client.sfu_client.request_count());

        std::thread::sleep(std::time::Duration::from_millis(1000));
        assert_eq!(1, client.sfu_client.request_count());

        // Eventually, we give up on the lack of a response and ask again.
        std::thread::sleep(std::time::Duration::from_millis(1000));
        assert_eq!(2, client.sfu_client.request_count());

        client.disconnect_and_wait_until_ended();
    }

    #[test]
    #[ignore]
    fn request_video() {
        use protobuf::group_call::{
            device_to_sfu::{
                video_request_message::VideoRequest as VideoRequestProto, VideoRequestMessage,
            },
            DeviceToSfu,
        };

        let mut client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        let client3 = TestClient::new(vec![3], 3);
        let client4 = TestClient::new(vec![4], 4);

        let (sender, receiver) = mpsc::channel();
        client1.sfu_rtp_packet_sender = Some(sender);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[&client2, &client3, &client4]);

        let requests = vec![
            VideoRequest {
                demux_id: 2,
                width: 1920,
                height: 1080,
                framerate: None,
            },
            VideoRequest {
                demux_id: 3,
                // Rotated!
                width: 80,
                height: 120,
                framerate: Some(5),
            },
            VideoRequest {
                demux_id: 4,
                width: 0,
                height: 0,
                framerate: None,
            },
            // This should be filtered out
            VideoRequest {
                demux_id: 5,
                width: 1000,
                height: 1000,
                framerate: None,
            },
        ];
        client1.client.request_video(requests.clone(), 0);
        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                video_request: Some(VideoRequestMessage {
                    requests: vec![
                        VideoRequestProto {
                            demux_id: Some(2),
                            height: Some(1080),
                        },
                        VideoRequestProto {
                            demux_id: Some(3),
                            height: Some(80),
                        },
                        VideoRequestProto {
                            demux_id: Some(4),
                            height: Some(0),
                        },
                    ],
                    max_kbps: Some(NORMAL_MAX_RECEIVE_RATE.as_kbps() as u32),
                    active_speaker_height: Some(0),
                }),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );

        client1.client.request_video(requests.clone(), 0);
        client1.client.request_video(requests.clone(), 0);
        client1.client.request_video(requests.clone(), 0);
        client1.client.request_video(requests.clone(), 0);

        let before = Instant::now();
        let _ = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Get RTP packet to SFU");
        let elapsed = Instant::now() - before;
        assert!(elapsed > Duration::from_millis(980));
        assert!(elapsed < Duration::from_millis(1020));

        client1.client.request_video(requests.clone(), 1080);
        client1.client.request_video(requests.clone(), 1080);
        client1.client.request_video(requests.clone(), 1080);
        client1.client.request_video(requests, 1080);

        let before = Instant::now();
        let _ = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Get RTP packet to SFU");
        let elapsed = Instant::now() - before;
        assert!(elapsed < Duration::from_millis(100));

        let before = Instant::now();
        let _ = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Get RTP packet to SFU");
        let elapsed = Instant::now() - before;
        assert!(elapsed > Duration::from_millis(1000));

        client1.client.set_data_mode(DataMode::Low);
        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                video_request: Some(VideoRequestMessage {
                    requests: vec![
                        VideoRequestProto {
                            demux_id: Some(2),
                            height: Some(1080),
                        },
                        VideoRequestProto {
                            demux_id: Some(3),
                            height: Some(80),
                        },
                        VideoRequestProto {
                            demux_id: Some(4),
                            height: Some(0),
                        },
                    ],
                    max_kbps: Some(500),
                    active_speaker_height: Some(1080),
                }),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );

        client1.client.set_data_mode(DataMode::Normal);
        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                video_request: Some(VideoRequestMessage {
                    requests: vec![
                        VideoRequestProto {
                            demux_id: Some(2),
                            height: Some(1080),
                        },
                        VideoRequestProto {
                            demux_id: Some(3),
                            height: Some(80),
                        },
                        VideoRequestProto {
                            demux_id: Some(4),
                            height: Some(0),
                        },
                    ],
                    max_kbps: Some(NORMAL_MAX_RECEIVE_RATE.as_kbps() as u32),
                    active_speaker_height: Some(1080),
                }),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn audio_level_polling() {
        let client1 = TestClient::new(vec![1], 1);
        assert_eq!(0, client1.observer.handle_audio_levels_invocation_count());
        client1.connect_join_and_wait_until_joined();
        assert_eq!(1, client1.observer.handle_audio_levels_invocation_count());
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(1, client1.observer.handle_audio_levels_invocation_count());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(1, client1.observer.handle_audio_levels_invocation_count());
    }

    #[test]
    fn device_to_sfu_leave() {
        use protobuf::group_call::{device_to_sfu::LeaveMessage, DeviceToSfu};

        let mut client1 = TestClient::new(vec![1], 1);

        let (sender, receiver) = mpsc::channel();
        client1.sfu_rtp_packet_sender = Some(sender);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[]);
        client1.client.leave();

        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                leave: Some(LeaveMessage {}),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );
    }

    #[test]
    fn device_to_sfu_remove() {
        use protobuf::group_call::{
            device_to_sfu::{AdminAction, GenericAdminAction},
            DeviceToSfu,
        };

        let mut client1 = TestClient::new(vec![1], 1);

        let (sender, receiver) = mpsc::channel();
        client1.sfu_rtp_packet_sender = Some(sender);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[]);
        client1.client.remove_client(32);

        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                admin_action: Some(AdminAction::Remove(GenericAdminAction {
                    target_demux_id: Some(32)
                })),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );
    }

    #[test]
    fn device_to_sfu_block() {
        use protobuf::group_call::{
            device_to_sfu::{AdminAction, GenericAdminAction},
            DeviceToSfu,
        };

        let mut client1 = TestClient::new(vec![1], 1);

        let (sender, receiver) = mpsc::channel();
        client1.sfu_rtp_packet_sender = Some(sender);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[]);
        client1.client.block_client(32);

        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                admin_action: Some(AdminAction::Block(GenericAdminAction {
                    target_demux_id: Some(32)
                })),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );
    }

    #[test]
    fn device_to_sfu_approve() {
        use protobuf::group_call::{
            device_to_sfu::{AdminAction, GenericAdminAction},
            DeviceToSfu,
        };

        let mut client1 = TestClient::new(vec![1], 1);

        let remote1 = TestClient::new(vec![11], 16);
        let remote2a = TestClient::new(vec![22], 32);
        let remote2b = TestClient::new(vec![22], 48);

        let (sender, receiver) = mpsc::channel();
        client1.sfu_rtp_packet_sender = Some(sender);
        client1.connect_join_and_wait_until_joined();
        client1.set_pending_clients_and_wait_until_applied(&[&remote1, &remote2a, &remote2b]);
        client1.client.approve_user(vec![22]);

        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                admin_action: Some(AdminAction::Approve(GenericAdminAction {
                    target_demux_id: Some(32)
                })),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );
    }

    #[test]
    fn approve_not_found() {
        let mut client1 = TestClient::new(vec![1], 1);

        let remote1 = TestClient::new(vec![11], 16);
        let remote2a = TestClient::new(vec![22], 32);
        let remote2b = TestClient::new(vec![22], 48);

        let (sender, receiver) = mpsc::channel();
        client1.sfu_rtp_packet_sender = Some(sender);
        client1.connect_join_and_wait_until_joined();
        client1.set_pending_clients_and_wait_until_applied(&[&remote1, &remote2a, &remote2b]);
        client1.client.approve_user(vec![33]);

        receiver
            .recv_timeout(Duration::from_millis(200))
            .expect_err("No packets to send");
    }

    #[test]
    fn device_to_sfu_deny() {
        use protobuf::group_call::{
            device_to_sfu::{AdminAction, GenericAdminAction},
            DeviceToSfu,
        };

        let mut client1 = TestClient::new(vec![1], 1);

        let remote1 = TestClient::new(vec![11], 16);
        let remote2a = TestClient::new(vec![22], 32);
        let remote2b = TestClient::new(vec![22], 48);

        let (sender, receiver) = mpsc::channel();
        client1.sfu_rtp_packet_sender = Some(sender);
        client1.connect_join_and_wait_until_joined();
        client1.set_pending_clients_and_wait_until_applied(&[&remote1, &remote2a, &remote2b]);
        client1.client.deny_user(vec![22]);

        let (header, payload) = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Get RTP packet to SFU");
        assert_eq!(1, header.ssrc);
        assert_eq!(
            DeviceToSfu {
                admin_action: Some(AdminAction::Deny(GenericAdminAction {
                    target_demux_id: Some(32)
                })),
                ..Default::default()
            },
            DeviceToSfu::decode(&payload[..]).unwrap()
        );
    }

    #[test]
    fn carry_over_devices_from_peeking_to_joined() {
        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        let client3 = TestClient::new(vec![3], 3);

        client1.client.set_membership_proof(b"proof".to_vec());
        client1.client.connect();
        client1.wait_for_client_to_process();

        client1.set_remotes_and_wait_until_applied(&[&client2, &client3]);
        assert_eq!(
            hash_set(vec![client2.user_id, client3.user_id]),
            hash_set(client1.observer.joined_members())
        );

        client1.client.join();
        client1.observer.joined.wait(Duration::from_secs(5));
        client1.wait_for_client_to_process();
        let remote_devices = client1.observer.remote_devices();
        assert_eq!(2, remote_devices.len());
        assert_eq!(2, remote_devices[0].demux_id);
        assert_eq!(3, remote_devices[1].demux_id);
        assert_eq!(
            client1.observer.remote_devices(),
            client1.observer.remote_devices_at_join_time(),
        );

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn era_id_populated_after_join() {
        let mut client1 = TestClient::new(vec![1], 1);

        client1.client.set_membership_proof(b"proof".to_vec());
        client1.client.connect();
        client1.wait_for_client_to_process();
        assert_eq!(None, client1.observer.peek_state().era_id);

        client1.default_peek_info = PeekInfo {
            era_id: Some("update me".to_string()),
            ..PeekInfo::default()
        };
        client1.set_remotes_and_wait_until_applied(&[]);
        assert_eq!(
            Some("update me"),
            client1.observer.peek_state().era_id.as_deref()
        );
        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn changing_group_members_triggers_poll() {
        let client1 = TestClient::new(vec![1], 1);
        client1.client.set_membership_proof(b"proof".to_vec());
        client1.client.connect();
        client1.wait_for_client_to_process();
        let initial_count = client1.sfu_client.request_count();
        let user_a = GroupMember {
            user_id: b"a".to_vec(),
            member_id: b"A".to_vec(),
        };
        let user_b = GroupMember {
            user_id: b"b".to_vec(),
            member_id: b"B".to_vec(),
        };
        client1.set_remotes_and_wait_until_applied(&[]);

        // Changing the list of group members triggers a poll
        client1
            .client
            .set_group_members(vec![user_a.clone(), user_b.clone()]);
        client1.wait_for_client_to_process();
        assert_eq!(initial_count + 1, client1.sfu_client.request_count());
        client1.set_remotes_and_wait_until_applied(&[]);

        // Setting the same list again - even in a different order - does not trigger a poll
        client1
            .client
            .set_group_members(vec![user_b, user_a.clone()]);
        client1.wait_for_client_to_process();
        assert_eq!(initial_count + 1, client1.sfu_client.request_count());

        // Setting a different list triggers a poll
        client1.client.set_group_members(vec![user_a]);
        client1.wait_for_client_to_process();
        assert_eq!(initial_count + 2, client1.sfu_client.request_count());

        client1.set_remotes_and_wait_until_applied(&[]);

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn full_call() {
        let client1 = TestClient::new(vec![1], 1);
        client1.client.connect();
        client1.client.set_peek_result(Ok(PeekInfo {
            devices: vec![PeekDeviceInfo {
                demux_id: 2,
                user_id: None,
            }],
            max_devices: Some(1),
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.client.join();
        assert_eq!(
            Some(EndReason::HasMaxDevices),
            client1.observer.ended.wait(Duration::from_secs(5))
        );

        let client1 = TestClient::new(vec![1], 1);
        client1.client.set_peek_result(Ok(PeekInfo {
            devices: vec![PeekDeviceInfo {
                demux_id: 2,
                user_id: None,
            }],
            max_devices: Some(2),
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.connect_join_and_wait_until_joined();
        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    #[ignore] // Because it's too slow
    fn membership_proof_requests() {
        let client1 = TestClient::new(vec![1], 1);
        client1.client.set_peek_result(Ok(PeekInfo {
            devices: vec![PeekDeviceInfo {
                demux_id: 2,
                user_id: None,
            }],
            max_devices: Some(2),
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        assert_eq!(
            0,
            client1.observer.request_membership_proof_invocation_count()
        );

        // Expect a request for connect and join.
        client1.connect_join_and_wait_until_joined();
        assert_eq!(
            2,
            client1.observer.request_membership_proof_invocation_count()
        );

        // TODO: Make Actors use tokio so we can use fake time
        std::thread::sleep(
            std::time::Duration::from_millis(2000) + MEMBERSHIP_PROOF_REQUEST_INTERVAL,
        );
        assert_eq!(
            1,
            client1.observer.request_membership_proof_invocation_count()
        );

        client1.disconnect_and_wait_until_ended();
        assert_eq!(
            0,
            client1.observer.request_membership_proof_invocation_count()
        );
    }

    #[test]
    fn speakers() {
        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        let client3 = TestClient::new(vec![3], 3);
        let client4 = TestClient::new(vec![4], 4);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[&client3, &client4]);
        assert_eq!(vec![3, 4], client1.speakers());
        assert_eq!(
            1,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // New people put at the end regardless of DemuxId
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.set_remotes_and_wait_until_applied(&[&client2, &client4, &client3]);
        assert_eq!(vec![3, 4, 2], client1.speakers());
        assert_eq!(
            1,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Changed
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(1, 4);
        assert_eq!(vec![4, 3, 2], client1.speakers());
        assert_eq!(
            1,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Didn't change
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(2, 4);
        assert_eq!(vec![4, 3, 2], client1.speakers());
        assert_eq!(
            0,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Changed back
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(3, 3);
        assert_eq!(vec![3, 4, 2], client1.speakers());
        assert_eq!(
            1,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Ignore unknown demux ID
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(4, 5);
        assert_eq!(vec![3, 4, 2], client1.speakers());
        assert_eq!(
            0,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Didn't change
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(6, 3);
        assert_eq!(vec![3, 4, 2], client1.speakers());
        assert_eq!(
            0,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Ignore old messages
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(5, 4);
        assert_eq!(vec![3, 4, 2], client1.speakers());
        assert_eq!(
            0,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Ignore when the local device is the current speaker
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(7, 1);
        assert_eq!(vec![3, 4, 2], client1.speakers());
        assert_eq!(
            0,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Finally give 2 a chance
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(8, 2);
        assert_eq!(vec![2, 3, 4], client1.speakers());
        assert_eq!(
            1,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Swap only the top two; leave the third alone
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(9, 3);
        assert_eq!(vec![3, 2, 4], client1.speakers());
        assert_eq!(
            1,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        // Unchanged
        std::thread::sleep(std::time::Duration::from_millis(1));
        client1.receive_speaker(10, 3);
        assert_eq!(vec![3, 2, 4], client1.speakers());
        assert_eq!(
            0,
            client1
                .observer
                .handle_remote_devices_changed_invocation_count()
        );

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn forwarding_video() {
        let get_forwarding_videos = |client: &TestClient| -> Vec<(DemuxId, Option<bool>, u16)> {
            client
                .observer
                .remote_devices()
                .iter()
                .map(|remote| {
                    (
                        remote.demux_id,
                        remote.forwarding_video,
                        remote.server_allocated_height,
                    )
                })
                .collect()
        };

        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        let client3 = TestClient::new(vec![3], 3);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[&client2, &client3]);

        assert_eq!(
            vec![(2, None, 0), (3, None, 0)],
            get_forwarding_videos(&client1)
        );

        client1
            .client
            .handle_forwarding_video_received(vec![2, 3], vec![240, 120]);
        client1.wait_for_client_to_process();

        assert_eq!(
            vec![(2, Some(true), 240), (3, Some(true), 120)],
            get_forwarding_videos(&client1)
        );

        client1
            .client
            .handle_forwarding_video_received(vec![2], vec![120]);
        client1.wait_for_client_to_process();

        assert_eq!(
            vec![(2, Some(true), 120), (3, Some(false), 0)],
            get_forwarding_videos(&client1)
        );

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn client_decoded_height() {
        let get_client_decoded_height = |client: &TestClient| -> Option<u32> {
            client
                .observer
                .remote_devices()
                .iter()
                .map(|remote| remote.client_decoded_height)
                .next()
                .unwrap()
        };
        let set_client_decoded_height = |client: &TestClient, height: u32| {
            let mut remote_devices = client.observer.remote_devices.lock().unwrap();
            remote_devices.get_mut(0).unwrap().client_decoded_height = Some(height);
        };

        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[&client2]);

        assert_eq!(None, get_client_decoded_height(&client1));

        client1
            .client
            .handle_forwarding_video_received(vec![2], vec![480]);
        client1.wait_for_client_to_process();

        set_client_decoded_height(&client1, 480);

        // There is no video when forwarding stops, so the height is None
        client1
            .client
            .handle_forwarding_video_received(vec![], vec![]);
        client1.wait_for_client_to_process();

        assert_eq!(None, get_client_decoded_height(&client1));

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn is_higher_resolution_pending() {
        let get_forwarding_videos = |client: &TestClient| -> Vec<(DemuxId, u16)> {
            client
                .observer
                .remote_devices()
                .iter()
                .map(|remote| (remote.demux_id, remote.server_allocated_height))
                .collect()
        };
        let set_client_decoded_height = |client: &TestClient, height: u32| {
            let mut remote_devices = client.observer.remote_devices.lock().unwrap();
            let mut device = remote_devices.get_mut(0).unwrap();
            device.client_decoded_height = Some(height);
            device.recalculate_higher_resolution_pending();
        };
        let is_higher_resolution_pending = |client: &TestClient| -> bool {
            let mut remote_devices = client.observer.remote_devices.lock().unwrap();
            remote_devices
                .get_mut(0)
                .unwrap()
                .is_higher_resolution_pending
        };

        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[&client2]);

        assert_eq!(vec![(2, 0)], get_forwarding_videos(&client1));
        assert!(!is_higher_resolution_pending(&client1));

        client1
            .client
            .handle_forwarding_video_received(vec![2], vec![240]);
        client1.wait_for_client_to_process();

        assert_eq!(vec![(2, 240)], get_forwarding_videos(&client1));

        // A higher resolution is pending because the server allocated a height of 240, but no
        // video has been decoded yet.
        assert!(is_higher_resolution_pending(&client1));

        // After receiving the higher resolution video, the pending status is cleared.
        set_client_decoded_height(&client1, 240);

        assert!(!is_higher_resolution_pending(&client1));

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn removal_before_approval() {
        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[&client2]);

        client1.client.handle_removed_received();
        assert_eq!(
            Some(EndReason::DeniedRequestToJoinCall),
            client1.observer.ended.wait(Duration::from_secs(5))
        );
    }

    #[test]
    fn removal_after_approval() {
        let client1 = TestClient::new(vec![1], 1);
        let client2 = TestClient::new(vec![2], 2);
        client1.connect_join_and_wait_until_joined();
        client1.set_remotes_and_wait_until_applied(&[&client2, &client1]);

        client1.client.handle_removed_received();
        assert_eq!(
            Some(EndReason::RemovedFromCall),
            client1.observer.ended.wait(Duration::from_secs(5))
        );
    }

    #[test]
    fn send_rates() {
        init_logging();
        let client1 = TestClient::new(vec![1], 1);
        client1.connect_join_and_wait_until_joined();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(1)),
            }),
            client1.observer.send_rates()
        );

        let devices: Vec<PeekDeviceInfo> = (1..=20)
            .map(|demux_id| {
                let user_id = format!("{}", demux_id);
                PeekDeviceInfo {
                    demux_id,
                    user_id: Some(user_id.as_bytes().to_vec()),
                }
            })
            .collect();
        client1.client.set_peek_result(Ok(PeekInfo {
            devices: vec![],
            max_devices: None,
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(1)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_peek_result(Ok(PeekInfo {
            devices: devices[..1].to_vec(),
            max_devices: None,
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(1)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_peek_result(Ok(PeekInfo {
            devices: devices[..2].to_vec(),
            max_devices: None,
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(1000)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_peek_result(Ok(PeekInfo {
            devices: devices[..5].to_vec(),
            max_devices: None,
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(1000)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_peek_result(Ok(PeekInfo {
            devices: devices[..20].to_vec(),
            max_devices: None,
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(671)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_sharing_screen(true);
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: Some(DataRate::from_kbps(2000)),
                start: Some(DataRate::from_kbps(2000)),
                max: Some(DataRate::from_kbps(5000)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_sharing_screen(false);
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(671)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_peek_result(Ok(PeekInfo {
            devices: devices[..0].to_vec(),
            max_devices: None,
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(1)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_sharing_screen(true);
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: None,
                start: None,
                max: Some(DataRate::from_kbps(1)),
            }),
            client1.observer.send_rates()
        );

        client1.client.set_peek_result(Ok(PeekInfo {
            devices: devices[..20].to_vec(),
            max_devices: None,
            pending_devices: vec![],
            creator: None,
            era_id: None,
        }));
        client1.wait_for_client_to_process();
        assert_eq!(
            Some(SendRates {
                min: Some(DataRate::from_kbps(2000)),
                start: Some(DataRate::from_kbps(2000)),
                max: Some(DataRate::from_kbps(5000)),
            }),
            client1.observer.send_rates()
        );

        client1.disconnect_and_wait_until_ended();
    }

    #[test]
    fn group_ring() {
        fn ring_once(era_id: &str) -> RingId {
            let user_id = vec![1];
            let demux_id = 1;

            let mut sfu_client = FakeSfuClient::new(demux_id, Some(user_id.clone()));
            sfu_client.era_id = era_id.to_string();

            let client1 = TestClient::with_sfu_client(user_id, demux_id, sfu_client);
            client1.connect_join_and_wait_until_joined();

            client1.client.ring(None);
            client1.wait_for_client_to_process();
            let sent_messages = std::mem::take(
                &mut *client1
                    .observer
                    .sent_group_signaling_messages
                    .lock()
                    .expect("finished processing"),
            );
            match &sent_messages[..] {
                [protobuf::signaling::CallMessage {
                    ring_intention: Some(ring),
                    ..
                }] => {
                    assert_eq!(
                        Some(protobuf::signaling::call_message::ring_intention::Type::Ring.into()),
                        ring.r#type,
                    );
                    ring.ring_id.expect("should have an ID").into()
                }
                _ => {
                    panic!(
                        "group messages not as expected; here's what we got: {:?}",
                        sent_messages
                    );
                }
            }
        }

        // Check that the ring IDs are derived from the era ID.
        let first_ring_id = ring_once("1122334455667788");
        let first_ring_id_again = ring_once("1122334455667788");
        assert_eq!(first_ring_id, first_ring_id_again);
        let second_ring_id = ring_once("99aabbccddeeff00");
        assert_ne!(first_ring_id, second_ring_id, "ring IDs were the same");

        // Check that non-hex era IDs are okay too, just in case.
        let non_hex_ring_id = ring_once("mesozoic");
        assert_ne!(first_ring_id, non_hex_ring_id, "ring IDs were the same");
    }

    #[test]
    fn group_ring_cancel() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id.clone(),
            demux_id,
            FakeSfuClient::new(demux_id, Some(user_id)),
        );
        client1.connect_join_and_wait_until_joined();
        client1.client.ring(None);
        client1.client.leave();
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        match &sent_messages[..] {
            [protobuf::signaling::CallMessage {
                ring_intention: Some(ring),
                ..
            }, protobuf::signaling::CallMessage {
                ring_intention: Some(cancel),
                ..
            }] => {
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Ring.into()),
                    ring.r#type,
                );
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Cancelled.into()),
                    cancel.r#type,
                );
                assert_eq!(ring.ring_id, cancel.ring_id, "ring IDs should be the same");
            }
            _ => {
                panic!(
                    "group messages not as expected; here's what we got: {:#?}",
                    sent_messages
                );
            }
        }
    }

    #[test]
    fn group_ring_no_cancel_if_someone_joins() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id.clone(),
            demux_id,
            FakeSfuClient::new(demux_id, Some(user_id)),
        );
        client1.connect_join_and_wait_until_joined();
        client1.client.ring(None);

        let client2 = TestClient::new(vec![2], 2);
        client1.set_remotes_and_wait_until_applied(&[&client2]);

        client1.client.leave();
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        match &sent_messages[..] {
            [protobuf::signaling::CallMessage {
                ring_intention: Some(ring),
                ..
            }] => {
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Ring.into()),
                    ring.r#type,
                );
            }
            _ => {
                panic!(
                    "group messages not as expected; here's what we got: {:#?}",
                    sent_messages
                );
            }
        }
    }

    #[test]
    fn group_ring_no_cancel_if_call_was_not_empty() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id.clone(),
            demux_id,
            FakeSfuClient::new(demux_id, Some(user_id)),
        );
        client1.connect_join_and_wait_until_joined();

        let client2 = TestClient::new(vec![2], 2);
        client1.set_remotes_and_wait_until_applied(&[&client2]);

        client1.client.ring(None);
        client1.client.leave();
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        match &sent_messages[..] {
            [protobuf::signaling::CallMessage {
                ring_intention: Some(ring),
                ..
            }] => {
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Ring.into()),
                    ring.r#type,
                );
            }
            _ => {
                panic!(
                    "group messages not as expected; here's what we got: {:#?}",
                    sent_messages
                );
            }
        }
    }

    #[test]
    fn group_ring_cancel_if_call_is_currently_empty() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id.clone(),
            demux_id,
            FakeSfuClient::new(demux_id, Some(user_id)),
        );
        client1.connect_join_and_wait_until_joined();

        let client2 = TestClient::new(vec![2], 2);
        client1.set_remotes_and_wait_until_applied(&[&client2]);
        client1.set_remotes_and_wait_until_applied(&[]);

        client1.client.ring(None);
        client1.client.leave();
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        match &sent_messages[..] {
            [protobuf::signaling::CallMessage {
                ring_intention: Some(ring),
                ..
            }, protobuf::signaling::CallMessage {
                ring_intention: Some(cancel),
                ..
            }] => {
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Ring.into()),
                    ring.r#type,
                );
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Cancelled.into()),
                    cancel.r#type,
                );
                assert_eq!(ring.ring_id, cancel.ring_id, "ring IDs should be the same");
            }
            _ => {
                panic!(
                    "group messages not as expected; here's what we got: {:#?}",
                    sent_messages
                );
            }
        }
    }

    #[test]
    fn group_ring_cancel_if_call_is_just_you() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id.clone(),
            demux_id,
            FakeSfuClient::new(demux_id, Some(user_id)),
        );
        client1.connect_join_and_wait_until_joined();

        client1.set_remotes_and_wait_until_applied(&[&client1]);

        client1.client.ring(None);
        client1.client.leave();
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        match &sent_messages[..] {
            [protobuf::signaling::CallMessage {
                ring_intention: Some(ring),
                ..
            }, protobuf::signaling::CallMessage {
                ring_intention: Some(cancel),
                ..
            }] => {
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Ring.into()),
                    ring.r#type,
                );
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Cancelled.into()),
                    cancel.r#type,
                );
                assert_eq!(ring.ring_id, cancel.ring_id, "ring IDs should be the same");
            }
            _ => {
                panic!(
                    "group messages not as expected; here's what we got: {:#?}",
                    sent_messages
                );
            }
        }
    }

    #[test]
    fn group_ring_not_sent_on_different_creator() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id,
            demux_id,
            FakeSfuClient::new(demux_id, Some(vec![2])),
        );
        client1.connect_join_and_wait_until_joined();
        client1.client.ring(None);
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        assert_eq!(&sent_messages, &[]);
    }

    #[test]
    fn group_ring_delayed_until_join() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id.clone(),
            demux_id,
            FakeSfuClient::new(demux_id, Some(user_id)),
        );
        client1.client.connect();
        client1.client.ring(None);
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        assert_eq!(&sent_messages, &[]);

        client1.connect_join_and_wait_until_joined();
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );

        match &sent_messages[..] {
            [protobuf::signaling::CallMessage {
                ring_intention: Some(ring),
                ..
            }] => {
                assert_eq!(
                    Some(protobuf::signaling::call_message::ring_intention::Type::Ring.into()),
                    ring.r#type,
                );
            }
            _ => {
                panic!(
                    "group messages not as expected; here's what we got: {:#?}",
                    sent_messages
                );
            }
        }
    }

    #[test]
    fn group_ring_delayed_with_different_creator() {
        let user_id = vec![1];
        let demux_id = 1;
        let client1 = TestClient::with_sfu_client(
            user_id,
            demux_id,
            FakeSfuClient::new(demux_id, Some(vec![2])),
        );
        client1.client.connect();
        client1.client.ring(None);
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        assert_eq!(&sent_messages, &[]);

        client1.connect_join_and_wait_until_joined();
        client1.wait_for_client_to_process();
        let sent_messages = std::mem::take(
            &mut *client1
                .observer
                .sent_group_signaling_messages
                .lock()
                .expect("finished processing"),
        );
        assert_eq!(&sent_messages, &[]);
    }
}

#[cfg(test)]
mod remote_devices_tests {
    use super::*;

    #[test]
    fn latest_speaker_of_empty_devices() {
        let remote_devices = RemoteDevices::default();
        assert_eq!(None, remote_devices.latest_speaker_demux_id());
    }

    #[test]
    fn latest_speaker_of_zero_speaking_devices() {
        let device_1 = remote_device_state(1, None);
        let device_2 = remote_device_state(2, None);
        let device_3 = remote_device_state(3, None);
        let remote_devices = RemoteDevices::from_iter(vec![device_1, device_2, device_3]);
        assert_eq!(None, remote_devices.latest_speaker_demux_id());
    }

    #[test]
    fn latest_speaker_of_multiple_speaking_devices() {
        let device_1 = remote_device_state(1, Some(time(100)));
        let device_2 = remote_device_state(2, Some(time(101)));
        let device_3 = remote_device_state(3, None);
        let remote_devices = RemoteDevices::from_iter(vec![device_1, device_2, device_3]);
        assert_eq!(Some(2), remote_devices.latest_speaker_demux_id());
    }

    #[test]
    fn find_by_demux_id_when_key_is_not_found() {
        let device_1 = remote_device_state(1, None);
        let device_2 = remote_device_state(2, None);
        let device_3 = remote_device_state(3, None);
        let absent_id = 4;
        let remote_devices = RemoteDevices::from_iter(vec![device_1, device_2, device_3]);
        let device_state = remote_devices.find_by_demux_id(absent_id);
        assert_eq!(None, device_state);
    }

    #[test]
    fn find_by_demux_id() {
        let device_1 = remote_device_state(1, None);
        let device_2 = remote_device_state(2, None);
        let device_3 = remote_device_state(3, None);
        let remote_devices = RemoteDevices::from_iter(vec![device_1, device_2.clone(), device_3]);
        assert_eq!(
            Some(&device_2),
            remote_devices.find_by_demux_id(device_2.demux_id)
        );
    }

    #[test]
    fn find_by_demux_id_mut_when_key_is_not_found() {
        let device_1 = remote_device_state(1, None);
        let device_2 = remote_device_state(2, None);
        let device_3 = remote_device_state(3, None);
        let absent_id = 4;
        let mut remote_devices = RemoteDevices::from_iter(vec![device_1, device_2, device_3]);
        let device_state = remote_devices.find_by_demux_id_mut(absent_id);
        assert_eq!(None, device_state);
    }

    #[test]
    fn find_by_demux_id_mut_and_edit_is_persisted() {
        let device_1 = remote_device_state(1, None);
        let device_2 = remote_device_state(2, None);
        let device_3 = remote_device_state(3, None);
        let device_2_demux_id = device_2.demux_id;
        let mut remote_devices = RemoteDevices::from_iter(vec![device_1, device_2, device_3]);
        let device_state = remote_devices
            .find_by_demux_id_mut(device_2_demux_id)
            .unwrap();
        device_state.speaker_time = Some(time(300));
        let device_state = remote_devices
            .find_by_demux_id_mut(device_2_demux_id)
            .unwrap();
        assert_eq!(Some(time(300)), device_state.speaker_time);
    }

    #[test]
    fn demux_id_set() {
        let device_1 = remote_device_state(1, None);
        let device_2 = remote_device_state(2, None);
        let device_3 = remote_device_state(3, None);
        let remote_devices = RemoteDevices::from_iter(vec![device_1, device_2, device_3]);
        assert_eq!(
            vec![1, 2, 3].into_iter().collect::<HashSet<_>>(),
            remote_devices.demux_id_set()
        );
    }

    fn time(timestamp: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp)
    }

    fn remote_device_state(id: u32, spoken_at: Option<SystemTime>) -> RemoteDeviceState {
        let mut remote_device_state =
            RemoteDeviceState::new(id, id.to_be_bytes().to_vec(), time(1));

        remote_device_state.speaker_time = spoken_at;

        remote_device_state
    }

    #[test]
    fn srtp_keys_from_master_key_material() {
        assert_eq!(
            SrtpKeys {
                client: SrtpKey {
                    suite: SrtpCryptoSuite::AeadAes128Gcm,
                    key: (1..=16).collect(),
                    salt: (17..=28).collect(),
                },
                server: SrtpKey {
                    suite: SrtpCryptoSuite::AeadAes128Gcm,
                    key: (29..=44).collect(),
                    salt: (45..=56).collect(),
                }
            },
            SrtpKeys::from_master_key_material(
                &((1..=56).collect::<Vec<u8>>().try_into().unwrap())
            )
        )
    }

    #[test]
    fn dhe_state() {
        struct NotCryptoRng<T: rand::RngCore>(T);

        impl<T: rand::RngCore> rand::RngCore for NotCryptoRng<T> {
            fn next_u32(&mut self) -> u32 {
                self.0.next_u32()
            }

            fn next_u64(&mut self) -> u64 {
                self.0.next_u64()
            }

            fn fill_bytes(&mut self, dest: &mut [u8]) {
                self.0.fill_bytes(dest)
            }

            fn try_fill_bytes(&mut self, dest: &mut [u8]) -> std::result::Result<(), rand::Error> {
                self.0.try_fill_bytes(dest)
            }
        }

        impl<T: rand::RngCore> rand::CryptoRng for NotCryptoRng<T> {}

        let mut rand = NotCryptoRng(rand::rngs::mock::StepRng::new(1, 1));
        let client_secret = EphemeralSecret::new(&mut rand);
        let server_secret = EphemeralSecret::new(&mut rand);
        let client_pub_key = PublicKey::from(&client_secret);
        let server_pub_key = PublicKey::from(&server_secret);
        let server_cert = &b"server_cert"[..];

        let mut state = DheState::default();
        assert!(matches!(state, DheState::NotYetStarted));
        state.negotiate_in_place(&server_pub_key, server_cert);
        assert!(matches!(state, DheState::NotYetStarted));

        state = DheState::start(client_secret);
        assert!(matches!(state, DheState::WaitingForServerPublicKey { .. }));
        state.negotiate_in_place(&server_pub_key, server_cert);
        assert!(matches!(state, DheState::Negotiated { .. }));
        if let DheState::Negotiated { srtp_keys } = state {
            let server_master_key_material = {
                // Code copied from the server
                let shared_secret = server_secret.diffie_hellman(&client_pub_key);
                let mut master_key_material = [0u8; 56];
                Hkdf::<Sha256>::new(Some(&[0u8; 32]), shared_secret.as_bytes())
                    .expand_multi_info(
                        &[
                            b"Signal_Group_Call_20211105_SignallingDH_SRTPKey_KDF",
                            server_cert,
                        ],
                        &mut master_key_material,
                    )
                    .unwrap();
                master_key_material
            };
            let expected_srtp_keys =
                SrtpKeys::from_master_key_material(&server_master_key_material);
            assert_eq!(expected_srtp_keys, srtp_keys);
        };
    }
}
