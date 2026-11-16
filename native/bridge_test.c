#include "bridge.c"
#include <assert.h>

bool LiGetHdrMetadata(PSS_HDR_METADATA metadata) { (void)metadata; return false; }
uint64_t LiGetMicroseconds(void) { return 0; }

static int reported;
static void feedback(void *ctx, int kind, int a, int b, int c, int d, const char *message) {
    (void)ctx; (void)b; (void)c; (void)d; (void)message;
    if (kind == 9) reported = a;
}

static void receive_frame(int peer, int type, void *body, size_t length) {
    struct frame h;
    assert(recv(peer, &h, sizeof h, MSG_WAITALL) == sizeof h);
    assert(h.type == type && h.length == length);
    assert(recv(peer, body, length, MSG_WAITALL) == (ssize_t)length);
}

int main(void) {
    int sv[2]; assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    assert(audio_configuration(0) == AUDIO_CONFIGURATION_STEREO);
    assert(audio_configuration(1) == AUDIO_CONFIGURATION_51_SURROUND);
    fd = sv[0]; notify = feedback; hdr_active = -1;
    assert(video_setup(VIDEO_FORMAT_H265_MAIN10, 1920, 1080, 60, NULL, 0) == 0);
    char picture[] = {0, 0, 0, 1, 0x26};
    LENTRY part = {.data = picture, .length = sizeof picture};
    DECODE_UNIT unit = {.frameType = FRAME_TYPE_IDR, .fullLength = sizeof picture, .bufferList = &part};
    assert(video_submit(&unit) == DR_NEED_IDR);
    char byte;
    assert(recv(sv[1], &byte, 1, MSG_DONTWAIT) < 0 && (errno == EAGAIN || errno == EWOULDBLOCK));
    OPUS_MULTISTREAM_CONFIGURATION opus = {.sampleRate = 48000, .channelCount = 6, .streams = 4, .coupledStreams = 2, .mapping = {0, 4, 1, 5, 2, 3}};
    assert(audio_setup(0, &opus, NULL, 0) == 0 && reported == 6);
    memset(&opus, 0, sizeof opus); /* Moonlight's callback storage may disappear. */
    assert(video_submit(&unit) == DR_OK);
    struct load_opus51 load;
    receive_frame(sv[1], 13, &load, sizeof load);
    const uint8_t expected[] = {0, 1, 4, 2, 3, 5};
    assert(load.load.channels == 6 && load.load.rate == 48000 && load.streams == 4 && load.coupled == 2);
    assert(!memcmp(load.mapping, expected, sizeof expected));
    char video[16 + sizeof picture]; receive_frame(sv[1], 2, video, sizeof video);
    unit.hdrActive = 1;
    assert(video_submit(&unit) == DR_OK);
    receive_frame(sv[1], 13, &load, sizeof load);
    assert(load.load.hdr == 1 && !memcmp(load.mapping, expected, sizeof expected));
    receive_frame(sv[1], 2, video, sizeof video);
    atomic_store(&loaded, true);
    char packet[] = {0x44, 0x33, 0x22, 0x11};
    audio_submit(packet, sizeof packet);
    char audio[16 + sizeof packet]; receive_frame(sv[1], 3, audio, sizeof audio);
    assert(!memcmp(audio + 16, packet, sizeof packet));
    atomic_store(&audio_ready, false); hdr_active = -1;
    OPUS_MULTISTREAM_CONFIGURATION uncoupled = {.sampleRate = 48000, .channelCount = 6, .streams = 6, .coupledStreams = 0, .mapping = {0, 1, 2, 3, 4, 5}};
    assert(audio_setup(0, &uncoupled, NULL, 0) == 0 && reported == 6);
    unit.hdrActive = 0;
    assert(video_submit(&unit) == DR_OK);
    receive_frame(sv[1], 13, &load, sizeof load);
    const uint8_t expected_uncoupled[] = {0, 2, 1, 4, 5, 3};
    assert(load.streams == 6 && load.coupled == 0 && !memcmp(load.mapping, expected_uncoupled, sizeof expected_uncoupled));
    receive_frame(sv[1], 2, video, sizeof video);
    atomic_store(&audio_ready, false);
    uncoupled.mapping[0] = uncoupled.mapping[1];
    assert(audio_setup(0, &uncoupled, NULL, 0) != 0);
    assert(video_submit(&unit) == DR_NEED_IDR);
    hdr_active = -1;
    OPUS_MULTISTREAM_CONFIGURATION stereo = {.sampleRate = 48000, .channelCount = 2, .streams = 1, .coupledStreams = 1, .mapping = {0, 1}};
    assert(audio_setup(0, &stereo, NULL, 0) == 0 && reported == 2);
    unit.hdrActive = 0;
    assert(video_submit(&unit) == DR_OK);
    struct load normal; receive_frame(sv[1], 1, &normal, sizeof normal);
    assert(normal.channels == 2);
    receive_frame(sv[1], 2, video, sizeof video);
    close(sv[0]); close(sv[1]);
    return 0;
}
