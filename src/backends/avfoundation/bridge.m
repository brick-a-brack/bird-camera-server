@import AVFoundation;
@import ImageIO;
@import Foundation;

#include "bridge.h"
#include <stdlib.h>
#include <string.h>

// ---------------------------------------------------------------------------
// WcFrameDelegate — stores the latest raw pixel buffer from the capture queue.
//
// JPEG encoding is done via CGBitmapContext + CGImageDestination (CPU/SIMD,
// no GPU pipeline) for predictable, stall-free latency.
// ---------------------------------------------------------------------------

@interface WcFrameDelegate : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
- (nullable NSData *)encodeLatestFrameAsJPEG;
@end

@implementation WcFrameDelegate {
    NSLock              *_lock;
    CVPixelBufferRef     _latestBuffer;
    dispatch_semaphore_t _firstFrameSem;
    BOOL                 _hasFrame;
}

- (instancetype)init {
    if ((self = [super init])) {
        _lock          = [[NSLock alloc] init];
        _latestBuffer  = NULL;
        _firstFrameSem = dispatch_semaphore_create(0);
        _hasFrame      = NO;
    }
    return self;
}

- (void)dealloc {
    [_lock lock];
    if (_latestBuffer) { CVPixelBufferRelease(_latestBuffer); _latestBuffer = NULL; }
    [_lock unlock];
}

// Runs on the serial captureQueue — must be as fast as possible.
- (void)captureOutput:(AVCaptureOutput *)output
didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
       fromConnection:(AVCaptureConnection *)connection {
    CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pb) return;

    CVPixelBufferRetain(pb);

    [_lock lock];
    if (_latestBuffer) CVPixelBufferRelease(_latestBuffer);
    _latestBuffer = pb;
    if (!_hasFrame) {
        _hasFrame = YES;
        dispatch_semaphore_signal(_firstFrameSem);
    }
    [_lock unlock];
}

// Called from the actor thread.
// Uses CGBitmapContext + ImageIO (CPU/NEON) — no GPU, no pipeline stalls.
- (nullable NSData *)encodeLatestFrameAsJPEG {
    if (!_hasFrame) {
        dispatch_semaphore_wait(_firstFrameSem,
            dispatch_time(DISPATCH_TIME_NOW, 2LL * NSEC_PER_SEC));
    }

    [_lock lock];
    CVPixelBufferRef pb = _latestBuffer ? CVPixelBufferRetain(_latestBuffer) : NULL;
    [_lock unlock];

    if (!pb) return nil;

    CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);

    size_t width       = CVPixelBufferGetWidth(pb);
    size_t height      = CVPixelBufferGetHeight(pb);
    size_t bytesPerRow = CVPixelBufferGetBytesPerRow(pb);
    void  *baseAddr    = CVPixelBufferGetBaseAddress(pb);

    // Wrap the BGRA pixel data in a CGImage without copying.
    // kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst matches
    // kCVPixelFormatType_32BGRA on little-endian (all Apple hardware).
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGContextRef bmpCtx = CGBitmapContextCreate(
        baseAddr, width, height, 8, bytesPerRow, cs,
        kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst);
    CGImageRef cgImage = CGBitmapContextCreateImage(bmpCtx);

    CGContextRelease(bmpCtx);
    CGColorSpaceRelease(cs);
    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    CVPixelBufferRelease(pb);

    if (!cgImage) return nil;

    NSMutableData *jpegData = [NSMutableData data];
    CGImageDestinationRef dest = CGImageDestinationCreateWithData(
        (__bridge CFMutableDataRef)jpegData,
        CFSTR("public.jpeg"),
        1, NULL);

    if (!dest) { CGImageRelease(cgImage); return nil; }

    CGImageDestinationAddImage(dest, cgImage, (__bridge CFDictionaryRef)@{
        (__bridge id)kCGImageDestinationLossyCompressionQuality: @(0.75)
    });
    CGImageDestinationFinalize(dest);
    CFRelease(dest);
    CGImageRelease(cgImage);

    return jpegData.length > 0 ? jpegData : nil;
}

@end

// ---------------------------------------------------------------------------
// WcSessionHandle — groups session + output + delegate + device
// ---------------------------------------------------------------------------

@interface WcSessionHandle : NSObject
@property (nonatomic, strong) AVCaptureSession  *session;
@property (nonatomic, strong) AVCaptureDevice   *device;
@property (nonatomic, strong) WcFrameDelegate   *delegate;
@property (nonatomic, strong) dispatch_queue_t   captureQueue;
@end

@implementation WcSessionHandle
@end

// ---------------------------------------------------------------------------
// Helper: append one option (no-op if already full)
// ---------------------------------------------------------------------------

static void push_option(WcParamDesc *p, int value, const char *label) {
    if (p->num_options >= WC_MAX_OPTIONS) return;
    p->options[p->num_options].value = value;
    strlcpy(p->options[p->num_options].label, label, WC_MAX_LABEL);
    p->num_options++;
}

// ---------------------------------------------------------------------------
// C interface — session management
// ---------------------------------------------------------------------------

int wc_list_devices(WcDeviceInfo *out, int capacity) {
    if (!out || capacity <= 0) return 0;

    NSArray<AVCaptureDeviceType> *types;
    if (@available(macOS 14.0, *)) {
        types = @[AVCaptureDeviceTypeBuiltInWideAngleCamera,
                  AVCaptureDeviceTypeExternal];
    } else {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        types = @[AVCaptureDeviceTypeBuiltInWideAngleCamera,
                  AVCaptureDeviceTypeExternalUnknown];
#pragma clang diagnostic pop
    }

    AVCaptureDeviceDiscoverySession *ds =
        [AVCaptureDeviceDiscoverySession
            discoverySessionWithDeviceTypes:types
                                  mediaType:AVMediaTypeVideo
                                   position:AVCaptureDevicePositionUnspecified];

    int count = 0;
    for (AVCaptureDevice *dev in ds.devices) {
        if (count >= capacity) break;
        strlcpy(out[count].unique_id, dev.uniqueID.UTF8String      ?: "", WC_MAX_STR);
        strlcpy(out[count].name,      dev.localizedName.UTF8String ?: "", WC_MAX_STR);
        count++;
    }
    return count;
}

void *wc_open_session(const char *unique_id) {
    NSString *uid = [NSString stringWithUTF8String:unique_id];
    AVCaptureDevice *device = [AVCaptureDevice deviceWithUniqueID:uid];
    if (!device) return NULL;

    NSError *error = nil;
    AVCaptureDeviceInput *input =
        [AVCaptureDeviceInput deviceInputWithDevice:device error:&error];
    if (!input) return NULL;

    dispatch_queue_t q =
        dispatch_queue_create("bird.avfoundation.capture", DISPATCH_QUEUE_SERIAL);

    WcFrameDelegate *delegate = [[WcFrameDelegate alloc] init];

    AVCaptureVideoDataOutput *output = [[AVCaptureVideoDataOutput alloc] init];
    output.videoSettings = @{
        (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA)
    };
    output.alwaysDiscardsLateVideoFrames = YES;
    [output setSampleBufferDelegate:delegate queue:q];

    AVCaptureSession *session = [[AVCaptureSession alloc] init];
    session.sessionPreset = AVCaptureSessionPreset1280x720;

    if (![session canAddInput:input] || ![session canAddOutput:output])
        return NULL;

    [session addInput:input];
    [session addOutput:output];
    [session startRunning];

    WcSessionHandle *handle = [[WcSessionHandle alloc] init];
    handle.session      = session;
    handle.device       = device;
    handle.delegate     = delegate;
    handle.captureQueue = q;

    return (__bridge_retained void *)handle;
}

void wc_close_session(void *handle) {
    if (!handle) return;
    WcSessionHandle *h = (__bridge_transfer WcSessionHandle *)handle;
    [h.session stopRunning];
}

int wc_capture_frame(void *handle, uint8_t **out_data, size_t *out_size) {
    if (!handle || !out_data || !out_size) return -1;

    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    NSData *jpeg = [h.delegate encodeLatestFrameAsJPEG];
    if (!jpeg || jpeg.length == 0) return -1;

    uint8_t *buf = (uint8_t *)malloc(jpeg.length);
    if (!buf) return -1;

    memcpy(buf, jpeg.bytes, jpeg.length);
    *out_data = buf;
    *out_size = jpeg.length;
    return 0;
}

void wc_free_frame(uint8_t *data) {
    free(data);
}

// ---------------------------------------------------------------------------
// C interface — parameter enumeration
// ---------------------------------------------------------------------------

int wc_get_parameters(void *handle, WcParamDesc *out, int capacity) {
    if (!handle || !out || capacity <= 0) return 0;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    AVCaptureDevice *dev = h.device;
    if (!dev) return 0;

    int count = 0;

    // --- Focus mode ---
    if (count < capacity) {
        WcParamDesc *p = &out[count];
        memset(p, 0, sizeof(*p));
        strlcpy(p->kind, "focus_mode", WC_MAX_KIND);
        p->current = (int)dev.focusMode;
        typedef struct { AVCaptureFocusMode m; const char *l; } FM;
        FM table[] = {
            { AVCaptureFocusModeLocked,              "Locked"         },
            { AVCaptureFocusModeAutoFocus,           "Auto"           },
            { AVCaptureFocusModeContinuousAutoFocus, "Continuous Auto"},
        };
        for (int i = 0; i < 3; i++)
            if ([dev isFocusModeSupported:table[i].m])
                push_option(p, (int)table[i].m, table[i].l);
        if (p->num_options >= 2) count++;
    }

    // --- Exposure mode ---
    if (count < capacity) {
        WcParamDesc *p = &out[count];
        memset(p, 0, sizeof(*p));
        strlcpy(p->kind, "exposure_mode", WC_MAX_KIND);
        p->current = (int)dev.exposureMode;
        typedef struct { AVCaptureExposureMode m; const char *l; } EM;
        EM table[] = {
            { AVCaptureExposureModeLocked,                "Locked"         },
            { AVCaptureExposureModeAutoExpose,            "Auto"           },
            { AVCaptureExposureModeContinuousAutoExposure,"Continuous Auto"},
        };
        for (int i = 0; i < 3; i++)
            if ([dev isExposureModeSupported:table[i].m])
                push_option(p, (int)table[i].m, table[i].l);
        if (p->num_options >= 2) count++;
    }

    // --- White balance mode ---
    if (count < capacity) {
        WcParamDesc *p = &out[count];
        memset(p, 0, sizeof(*p));
        strlcpy(p->kind, "white_balance_mode", WC_MAX_KIND);
        p->current = (int)dev.whiteBalanceMode;
        typedef struct { AVCaptureWhiteBalanceMode m; const char *l; } WM;
        WM table[] = {
            { AVCaptureWhiteBalanceModeLocked,                    "Locked"         },
            { AVCaptureWhiteBalanceModeAutoWhiteBalance,          "Auto"           },
            { AVCaptureWhiteBalanceModeContinuousAutoWhiteBalance,"Continuous Auto"},
        };
        for (int i = 0; i < 3; i++)
            if ([dev isWhiteBalanceModeSupported:table[i].m])
                push_option(p, (int)table[i].m, table[i].l);
        if (p->num_options >= 2) count++;
    }

    return count;
}

// ---------------------------------------------------------------------------
// C interface — parameter setting
// ---------------------------------------------------------------------------

int wc_set_parameter(void *handle, const char *kind, int value) {
    if (!handle || !kind) return -1;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    AVCaptureDevice *dev = h.device;
    if (!dev) return -1;

    NSError *error = nil;
    if (![dev lockForConfiguration:&error]) return -1;

    BOOL ok = YES;

    if (strcmp(kind, "focus_mode") == 0) {
        AVCaptureFocusMode m = (AVCaptureFocusMode)value;
        if ([dev isFocusModeSupported:m]) {
            dev.focusMode = m;
        } else { ok = NO; }

    } else if (strcmp(kind, "exposure_mode") == 0) {
        AVCaptureExposureMode m = (AVCaptureExposureMode)value;
        if ([dev isExposureModeSupported:m]) {
            dev.exposureMode = m;
        } else { ok = NO; }

    } else if (strcmp(kind, "white_balance_mode") == 0) {
        AVCaptureWhiteBalanceMode m = (AVCaptureWhiteBalanceMode)value;
        if ([dev isWhiteBalanceModeSupported:m]) {
            dev.whiteBalanceMode = m;
        } else { ok = NO; }

    } else {
        ok = NO;
    }

    [dev unlockForConfiguration];
    return ok ? 0 : -1;
}
