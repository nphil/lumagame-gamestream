/* GPL-3.0-only. LumaGame's callbacks around unmodified moonlight-common-c.
 * gf_proto v2 is an independent socket protocol, not a link to the MIT TV process. */
#include <Limelight.h>
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error gf_proto requires a little-endian build
#endif
#define MAX_PAYLOAD (8u << 20)
typedef void (*notify_fn)(void *, int, int, int, int, int, const char *);
struct gs_config {
    const char *address, *version, *gfe_version, *rtsp_url, *feeder;
    int width, height, fps, bitrate, format, server_codecs, surround;
    unsigned char key[16], iv[16];
};
struct frame { uint8_t type, flags; uint16_t reserved; uint32_t length; };
struct media { int64_t pts; uint8_t key, pad[7]; };
struct load { uint32_t codec, width, height, fps, audio, channels, rate, hdr, metadata[15]; int32_t drop, pts_unit; };
struct load_opus51 { struct load load; uint8_t streams, coupled, mapping[6]; };
struct gs_stats { uint64_t frames, bytes, client_us, host_tenths, host_samples; uint32_t rtt; int rtt_valid; };
_Static_assert(sizeof(struct frame) == 8, "gf_frame");
_Static_assert(sizeof(struct media) == 16, "gf_media_head");
_Static_assert(sizeof(struct load) == 100, "gf_load");
_Static_assert(sizeof(struct load_opus51) == 108, "gf_load_opus51");

static int fd = -1, reader_started, started;
static pthread_t reader;
static pthread_mutex_t writer = PTHREAD_MUTEX_INITIALIZER;
static notify_fn notify;
static void *notify_context;
static atomic_bool closing, failed, connected, loaded, accepted, announced, measuring;
static atomic_bool load_sent, hdr_changed, mode_changed, key_requested;
static atomic_bool audio_ready;
static OPUS_MULTISTREAM_CONFIGURATION audio_config;
static atomic_ullong load_time;
static atomic_ullong stat_frames, stat_bytes, stat_client, stat_host, stat_host_samples;
static int video_format, video_width, video_height, video_fps;
static atomic_int hdr_active;
static uint32_t last_hdr[15];
static uint64_t now_us(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000 + t.tv_nsec / 1000; }
static int audio_configuration(int surround) { return surround ? AUDIO_CONFIGURATION_51_SURROUND : AUDIO_CONFIGURATION_STEREO; }
static void tell(int kind, int a, int b, int c, int d, const char *message) { if (notify) notify(notify_context, kind, a, b, c, d, message ? message : ""); }
static void fail(const char *message) { if (!atomic_exchange(&failed, true) && !atomic_load(&closing)) tell(4, 0, 0, 0, 0, message); }
static void running(void) {
    if (atomic_load(&connected) && atomic_load(&loaded) && atomic_load(&accepted)) {
        if (!atomic_exchange(&announced, true)) tell(2, atomic_load(&hdr_active) == 1, 0, 0, 0, "");
        else if (atomic_exchange(&mode_changed, false)) tell(6, atomic_load(&hdr_active) == 1, 0, 0, 0, "");
    }
}
static int read_all(void *data, size_t size) {
    char *p = data;
    while (size) { ssize_t n = recv(fd, p, size, 0); if (n < 0 && errno == EINTR) continue; if (n <= 0) return -1; p += n; size -= (size_t)n; }
    return 0;
}
static int write_parts(struct iovec *parts, int count) {
    int first = 0;
    while (first < count) {
        struct msghdr message = { .msg_iov = parts + first, .msg_iovlen = (size_t)(count - first) };
        ssize_t n = sendmsg(fd, &message, MSG_NOSIGNAL);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        while (first < count && (size_t)n >= parts[first].iov_len) { n -= (ssize_t)parts[first].iov_len; first++; }
        if (first < count) { parts[first].iov_base = (char *)parts[first].iov_base + n; parts[first].iov_len -= (size_t)n; }
    }
    return 0;
}
static int packet_locked(int type, const void *data, size_t size) {
    struct frame head = { .type = (uint8_t)type, .length = (uint32_t)size };
    struct iovec parts[2] = {{&head, sizeof(head)}, {(void *)data, size}};
    return write_parts(parts, size ? 2 : 1);
}
static void hdr_metadata(uint32_t out[15]) {
    memset(out, 0, 15 * sizeof(*out));
    SS_HDR_METADATA h;
    if (LiGetHdrMetadata(&h)) {
        /* Moonlight is RGB; NDL follows ST 2086's green, blue, red order. */
        const int order[3] = {1, 2, 0};
        for (int i = 0; i < 3; i++) { out[2*i] = h.displayPrimaries[order[i]].x; out[2*i+1] = h.displayPrimaries[order[i]].y; }
        out[6] = h.whitePoint.x; out[7] = h.whitePoint.y;
        out[8] = (uint32_t)h.maxDisplayLuminance * 10000; out[9] = h.minDisplayLuminance;
        out[10] = h.maxContentLightLevel; out[11] = h.maxFrameAverageLightLevel;
    }
    out[12] = 16; out[13] = 9; out[14] = 9;
}
static int send_load(int hdr) {
    struct load_opus51 message = { .load = { .width = video_width, .height = video_height, .fps = video_fps, .audio = 1, .channels = audio_config.channelCount, .rate = 48000, .hdr = hdr, .drop = -1, .pts_unit = 1000000 } };
    struct load l = message.load;
    l.codec = (video_format & VIDEO_FORMAT_MASK_H264) ? 1 : (video_format & VIDEO_FORMAT_MASK_H265) ? 2 : 4;
    if (hdr) hdr_metadata(l.metadata);
    memcpy(last_hdr, l.metadata, sizeof(last_hdr));
    atomic_store(&loaded, false);
    atomic_store(&accepted, false);
    atomic_store(&load_time, now_us());
    atomic_store(&load_sent, true);
    hdr_active = hdr;
    message.load = l;
    if (audio_config.channelCount == 6) {
        static const unsigned order[6] = {0, 2, 1, 4, 5, 3};
        message.streams = audio_config.streams;
        message.coupled = audio_config.coupledStreams;
        for (unsigned i = 0; i < 6; i++) message.mapping[i] = audio_config.mapping[order[i]];
        return packet_locked(13, &message, sizeof(message));
    }
    return packet_locked(1, &l, sizeof(l));
}
static int video_setup(int format, int width, int height, int fps, void *context, int flags) {
    (void)context; (void)flags;
    if (!(format & (VIDEO_FORMAT_H264 | VIDEO_FORMAT_H265 | VIDEO_FORMAT_H265_MAIN10 | VIDEO_FORMAT_AV1_MAIN8)) || ((format & VIDEO_FORMAT_MASK_AV1) && fps > 60)) return -1;
    video_format = format; video_width = width; video_height = height; video_fps = fps;
    tell(1, format, width, height, fps, "");
    return 0;
}
static int video_submit(PDECODE_UNIT unit) {
    if (atomic_load(&failed) || atomic_load(&closing)) return DR_NEED_IDR;
    if (!atomic_load_explicit(&audio_ready, memory_order_acquire)) return DR_NEED_IDR;
    if (unit->fullLength <= 0 || (unsigned)unit->fullLength > MAX_PAYLOAD - sizeof(struct media) - 2) { fail("A video frame exceeds the feeder's 8 MiB limit"); return DR_NEED_IDR; }
    int hdr = unit->hdrActive && (video_format & VIDEO_FORMAT_MASK_10BIT);
    uint64_t clock = now_us();
    int rc = 0;
    pthread_mutex_lock(&writer);
    if (hdr_active != hdr) {
        if (unit->frameType != FRAME_TYPE_IDR) { pthread_mutex_unlock(&writer); return DR_NEED_IDR; }
        rc = send_load(hdr);
        atomic_store(&mode_changed, atomic_load(&announced));
        atomic_store(&hdr_changed, false);
    } else if (hdr && atomic_exchange(&hdr_changed, false)) {
        uint32_t metadata[15]; hdr_metadata(metadata);
        if (memcmp(metadata, last_hdr, sizeof(metadata))) { rc = packet_locked(7, metadata, sizeof(metadata)); memcpy(last_hdr, metadata, sizeof(metadata)); }
    }
    struct frame header = { .type = 2, .length = (uint32_t)unit->fullLength + sizeof(struct media) };
    struct media media = { .pts = (int64_t)(clock * 1000), .key = unit->frameType == FRAME_TYPE_IDR };
    struct iovec parts[64] = {{&header, sizeof(header)}, {&media, sizeof(media)}};
    int count = 2;
    /* NDL expects low-overhead AV1 temporal units. A delimiter is a separate iovec, never a frame-sized copy. */
    unsigned char delimiter[2] = {0x12, 0};
    if (video_format & VIDEO_FORMAT_MASK_AV1) {
        unsigned char first[2] = {0}; size_t used = 0;
        for (PLENTRY p = unit->bufferList; p && used < 2; p = p->next) {
            for (int i = 0; i < p->length && used < 2; i++) first[used++] = (unsigned char)p->data[i];
        }
        if (used < 2 || memcmp(first, delimiter, 2)) { header.length += 2; parts[count++] = (struct iovec){delimiter, 2}; }
    }
    size_t total = 0;
    for (PLENTRY p = unit->bufferList; p; p = p->next) {
        if (p->length <= 0 || (size_t)p->length > (size_t)unit->fullLength - total) { rc = -1; break; }
        total += p->length;
        parts[count++] = (struct iovec){p->data, (size_t)p->length};
        if (count == 64) { if (!rc) rc = write_parts(parts, count); count = 0; }
    }
    if (total != (size_t)unit->fullLength) rc = -1;
    if (!rc && count) rc = write_parts(parts, count);
    pthread_mutex_unlock(&writer);
    if (rc) { fail("The TV feeder stopped accepting video"); return DR_NEED_IDR; }
    if (unit->frameType == FRAME_TYPE_IDR) atomic_store(&key_requested, false);
    if (atomic_load(&measuring)) {
        atomic_fetch_add(&stat_frames, 1); atomic_fetch_add(&stat_bytes, (unsigned)unit->fullLength);
        uint64_t submitted = LiGetMicroseconds();
        if (submitted >= unit->enqueueTimeUs) atomic_fetch_add(&stat_client, submitted - unit->enqueueTimeUs);
        if (unit->frameHostProcessingLatency) { atomic_fetch_add(&stat_host, unit->frameHostProcessingLatency); atomic_fetch_add(&stat_host_samples, 1); }
    }
    return DR_OK;
}
static int audio_setup(int config, const POPUS_MULTISTREAM_CONFIGURATION opus, void *context, int flags) {
    (void)config; (void)context; (void)flags;
    if (!opus || opus->sampleRate != 48000) return -1;
    if (opus->channelCount == 2) {
        if (opus->streams != 1 || opus->coupledStreams != 1 || opus->mapping[0] != 0 || opus->mapping[1] != 1) return -1;
    } else if (opus->channelCount == 6) {
        if (opus->streams < 1 || opus->streams > 6 || opus->coupledStreams < 0 ||
            opus->coupledStreams > opus->streams || opus->streams + opus->coupledStreams != 6) return -1;
        unsigned mask = 0;
        for (unsigned i = 0; i < 6; i++) {
            if (opus->mapping[i] >= 6) return -1;
            mask |= 1u << opus->mapping[i];
        }
        if (mask != 63) return -1;
    } else return -1;
    audio_config = *opus;
    tell(9, opus->channelCount, 0, 0, 0, "");
    atomic_store_explicit(&audio_ready, true, memory_order_release);
    return 0;
}
static void audio_submit(char *data, int size) {
    if (!atomic_load(&loaded) || atomic_load(&failed) || atomic_load(&closing) || size <= 0 || (unsigned)size > MAX_PAYLOAD - 16) return;
    struct frame header = { .type = 3, .length = (uint32_t)size + 16 };
    struct media media = { .pts = (int64_t)(now_us() * 1000) };
    struct iovec parts[3] = {{&header, 8}, {&media, 16}, {data, (size_t)size}};
    pthread_mutex_lock(&writer); int rc = write_parts(parts, 3); pthread_mutex_unlock(&writer);
    if (rc) fail("The TV feeder stopped accepting audio");
}
static void connection_started(void) { atomic_store(&connected, true); running(); }
static void connection_terminated(int code) { tell(3, code, 0, 0, 0, ""); }
static void stage_failed(int stage, int code) { tell(7, code, 0, 0, 0, LiGetStageName(stage)); }
static void stage_starting(int stage) { tell(8, stage, 0, 0, 0, ""); }
static void hdr_updated(bool enabled) { (void)enabled; atomic_store(&hdr_changed, true); }
static void rumble(unsigned short slot, unsigned short low, unsigned short high) { tell(5, slot, low, high, 0, ""); }
static void log_message(const char *format, ...) { va_list args; va_start(args, format); vfprintf(stderr, format, args); va_end(args); }
static void *read_feedback(void *unused) {
    (void)unused;
    unsigned char payload[8192];
    while (!atomic_load(&closing)) {
        struct pollfd pfd = {.fd = fd, .events = POLLIN};
        int ready = poll(&pfd, 1, 1000);
        if (ready < 0 && errno == EINTR) continue;
        if (!ready) {
            if (atomic_load(&load_sent) && !atomic_load(&loaded) && now_us() - atomic_load(&load_time) > 15000000) { fail("The TV player did not finish loading within 15 seconds"); break; }
            continue;
        }
        struct frame header;
        if (ready < 0 || read_all(&header, sizeof(header))) break;
        if (header.length > MAX_PAYLOAD) { fail("The TV feeder sent an oversized message"); break; }
        uint32_t saved = header.length < sizeof(payload)-1 ? header.length : sizeof(payload)-1;
        if (read_all(payload, saved)) break;
        payload[saved] = 0;
        uint32_t rest = header.length - saved;
        char discard[1024];
        while (rest) { unsigned n = rest > sizeof(discard) ? sizeof(discard) : rest; if (read_all(discard, n)) goto closed; rest -= n; }
        int32_t value = 0; if (saved >= 4) memcpy(&value, payload, 4);
        if (header.type == 16 && saved >= 4 && value != 1) { fail((char *)payload + 4); break; }
        if (header.type == 17 && saved >= 16 && value == 22) { atomic_store(&loaded, true); running(); }
        if (header.type == 18 && saved >= 16) { atomic_store(&accepted, true); running(); }
        int32_t reason = 0; if (header.type == 19 && saved >= 16) memcpy(&reason, payload + 8, 4);
        if (header.type == 24 || (header.type == 19 && reason >= 1 && reason <= 3)) {
            if (atomic_load(&connected) && !atomic_exchange(&key_requested, true)) LiRequestIdrFrame();
        }
        if (header.type == 21 && saved >= 4) { fail((char *)payload + 4); break; }
    }
closed:
    if (!atomic_load(&closing)) fail("The TV feeder connection closed");
    return NULL;
}
static void close_feeder(void) {
    atomic_store(&closing, true);
    if (fd >= 0) shutdown(fd, SHUT_RDWR);
    if (reader_started) { pthread_join(reader, NULL); reader_started = 0; }
    if (fd >= 0) { close(fd); fd = -1; }
}
int gs_start(const struct gs_config *c, notify_fn callback, void *context) {
    notify = callback; notify_context = context;
    atomic_store(&closing, false); atomic_store(&failed, false); atomic_store(&connected, false);
    atomic_store(&loaded, false); atomic_store(&accepted, false); atomic_store(&announced, false); atomic_store(&load_sent, false);
    atomic_store(&measuring, false); hdr_active = -1; started = 0;
    atomic_store(&hdr_changed, false); atomic_store(&mode_changed, false); atomic_store(&key_requested, false);
    atomic_store(&audio_ready, false);
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    if (strlen(c->feeder) >= sizeof(addr.sun_path)) { fail("The feeder socket path is too long"); return -1; }
    strcpy(addr.sun_path, c->feeder);
    fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0 || connect(fd, (struct sockaddr *)&addr, sizeof(addr))) { fail("Could not connect to the TV feeder"); close_feeder(); return -1; }
    struct timeval timeout = {.tv_sec = 5}; setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    timeout.tv_sec = 0; timeout.tv_usec = 500000; setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
    struct frame h; uint32_t hello[28];
    if (read_all(&h, 8)) { fail("The TV feeder did not send its greeting"); close_feeder(); return -1; }
    if (h.type == 21 && h.length >= 4 && h.length <= 1024) {
        int32_t code; char message[1021] = {0}, error[1100];
        if (!read_all(&code, 4) && !read_all(message, h.length - 4)) {
            snprintf(error, sizeof(error), "TV feeder error %d: %s", code, message); fail(error);
        } else fail("The TV feeder closed while reporting an error");
        close_feeder(); return -1;
    }
    if (h.type != 23 || h.length != sizeof(hello) || read_all(hello, sizeof(hello)) || hello[0] != 2 || hello[1] != 1) { fail("The TV feeder has no compatible video player"); close_feeder(); return -1; }
    timeout.tv_usec = 0; setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    if (pthread_create(&reader, NULL, read_feedback, NULL)) { fail("Could not start the feeder reader"); close_feeder(); return -1; }
    reader_started = 1;
    SERVER_INFORMATION server; LiInitializeServerInformation(&server);
    server.address = c->address; server.serverInfoAppVersion = c->version; server.serverInfoGfeVersion = c->gfe_version; server.rtspSessionUrl = c->rtsp_url; server.serverCodecModeSupport = c->server_codecs;
    STREAM_CONFIGURATION stream; LiInitializeStreamConfiguration(&stream);
    stream.width = c->width; stream.height = c->height; stream.fps = c->fps; stream.bitrate = c->bitrate;
    stream.packetSize = 1392; stream.streamingRemotely = STREAM_CFG_AUTO;
    stream.audioConfiguration = audio_configuration(c->surround); stream.supportedVideoFormats = c->format;
    stream.clientRefreshRateX100 = c->fps * 100; stream.colorSpace = (c->format & VIDEO_FORMAT_MASK_10BIT) ? COLORSPACE_REC_2020 : COLORSPACE_REC_709;
    stream.colorRange = COLOR_RANGE_LIMITED; stream.encryptionFlags = ENCFLG_ALL;
    memcpy(stream.remoteInputAesKey, c->key, 16); memcpy(stream.remoteInputAesIv, c->iv, 16);
    DECODER_RENDERER_CALLBACKS video; LiInitializeVideoCallbacks(&video);
    video.setup = video_setup; video.submitDecodeUnit = video_submit;
    /* Socket backpressure can block. DIRECT_SUBMIT would block Moonlight's receive thread and cause packet loss. */
    AUDIO_RENDERER_CALLBACKS audio; LiInitializeAudioCallbacks(&audio);
    audio.init = audio_setup; audio.decodeAndPlaySample = audio_submit; audio.capabilities = CAPABILITY_SUPPORTS_ARBITRARY_AUDIO_DURATION;
    CONNECTION_LISTENER_CALLBACKS events; LiInitializeConnectionCallbacks(&events);
    events.connectionStarted = connection_started; events.connectionTerminated = connection_terminated; events.stageFailed = stage_failed;
    events.rumble = rumble; events.logMessage = log_message;
    events.stageStarting = stage_starting; events.setHdrMode = hdr_updated;
    int rc = LiStartConnection(&server, &stream, &events, &video, &audio, NULL, 0, NULL, 0);
    started = rc == 0;
    if (rc) close_feeder();
    return rc;
}
void gs_interrupt(void) { LiInterruptConnection(); }
void gs_stop(void) {
    atomic_store(&closing, true);
    if (fd >= 0) shutdown(fd, SHUT_RDWR);
    if (reader_started) { pthread_join(reader, NULL); reader_started = 0; }
    atomic_store(&connected, false);
    if (started) { LiStopConnection(); started = 0; }
    close_feeder(); notify = NULL; notify_context = NULL;
}
void gs_controller(uint8_t slot, uint16_t mask, uint16_t buttons, uint8_t lt, uint8_t rt, int16_t lx, int16_t ly, int16_t rx, int16_t ry) {
    LiSendMultiControllerEvent(slot, mask, buttons, lt, rt, lx, ly, rx, ry);
}
void gs_set_stats(int on) {
    atomic_store(&stat_frames, 0); atomic_store(&stat_bytes, 0); atomic_store(&stat_client, 0); atomic_store(&stat_host, 0); atomic_store(&stat_host_samples, 0);
    atomic_store(&measuring, on != 0);
}
void gs_get_stats(struct gs_stats *out) {
    memset(out, 0, sizeof(*out));
    out->frames = atomic_exchange(&stat_frames, 0); out->bytes = atomic_exchange(&stat_bytes, 0); out->client_us = atomic_exchange(&stat_client, 0);
    out->host_tenths = atomic_exchange(&stat_host, 0); out->host_samples = atomic_exchange(&stat_host_samples, 0);
    uint32_t variance; out->rtt_valid = LiGetEstimatedRttInfo(&out->rtt, &variance);
}
const char *gs_launch_extensions(void) { return LiGetLaunchUrlQueryParameters(); }
