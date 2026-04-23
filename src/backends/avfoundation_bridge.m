@import AVFoundation;
@import ImageIO;
@import Foundation;

#include "avfoundation_bridge.h"
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
// WcSessionHandle — groups session + output + delegate into one object
// ---------------------------------------------------------------------------

@interface WcSessionHandle : NSObject
@property (nonatomic, strong) AVCaptureSession  *session;
@property (nonatomic, strong) WcFrameDelegate   *delegate;
@property (nonatomic, strong) dispatch_queue_t   captureQueue;
@end

@implementation WcSessionHandle
@end

// ---------------------------------------------------------------------------
// C interface
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
