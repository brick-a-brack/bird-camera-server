@import AVFoundation;
@import CoreImage;
@import Foundation;

#include "avfoundation_bridge.h"
#include <stdlib.h>
#include <string.h>

// ---------------------------------------------------------------------------
// WcFrameDelegate — receives sample buffers and stores the latest JPEG
// ---------------------------------------------------------------------------

@interface WcFrameDelegate : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
- (nullable NSData *)copyLatestFrame;
@end

@implementation WcFrameDelegate {
    NSLock       *_lock;
    CIContext    *_ctx;
    NSData       *_latestJpeg;
    // Signals once when the very first frame arrives so wc_capture_frame can
    // block briefly rather than returning nil immediately after session start.
    dispatch_semaphore_t _firstFrameSem;
    BOOL _hasFrame;
}

- (instancetype)init {
    if ((self = [super init])) {
        _lock          = [[NSLock alloc] init];
        _ctx           = [CIContext contextWithOptions:nil];
        _firstFrameSem = dispatch_semaphore_create(0);
        _hasFrame      = NO;
    }
    return self;
}

- (void)captureOutput:(AVCaptureOutput *)output
didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
       fromConnection:(AVCaptureConnection *)connection {
    CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pb) return;

    CIImage *image = [CIImage imageWithCVPixelBuffer:pb];
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    NSData *jpeg = [_ctx JPEGRepresentationOfImage:image
                                        colorSpace:cs
                                           options:@{}];
    CGColorSpaceRelease(cs);
    if (!jpeg) return;

    [_lock lock];
    _latestJpeg = jpeg;
    if (!_hasFrame) {
        _hasFrame = YES;
        dispatch_semaphore_signal(_firstFrameSem);
    }
    [_lock unlock];
}

- (nullable NSData *)copyLatestFrame {
    // Wait up to 2 s for the first frame after session start.
    if (!_hasFrame) {
        dispatch_semaphore_wait(_firstFrameSem,
            dispatch_time(DISPATCH_TIME_NOW, 2LL * NSEC_PER_SEC));
    }
    [_lock lock];
    NSData *copy = [_latestJpeg copy];
    [_lock unlock];
    return copy;
}

@end

// ---------------------------------------------------------------------------
// WcSessionHandle — groups session + output + delegate into one object
// ---------------------------------------------------------------------------

@interface WcSessionHandle : NSObject
@property (nonatomic, strong) AVCaptureSession        *session;
@property (nonatomic, strong) WcFrameDelegate         *delegate;
@property (nonatomic, strong) dispatch_queue_t         captureQueue;
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
    NSString *uid    = [NSString stringWithUTF8String:unique_id];
    AVCaptureDevice *device = [AVCaptureDevice deviceWithUniqueID:uid];
    if (!device) return NULL;

    NSError *error = nil;
    AVCaptureDeviceInput *input =
        [AVCaptureDeviceInput deviceInputWithDevice:device error:&error];
    if (!input) return NULL;

    dispatch_queue_t q =
        dispatch_queue_create("bird.webcam.capture", DISPATCH_QUEUE_SERIAL);

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

    WcSessionHandle *handle  = [[WcSessionHandle alloc] init];
    handle.session      = session;
    handle.delegate     = delegate;
    handle.captureQueue = q;

    // Transfer ownership to the caller — balanced by __bridge_transfer in wc_close_session.
    return (__bridge_retained void *)handle;
}

void wc_close_session(void *handle) {
    if (!handle) return;
    // Consume the retained reference created in wc_open_session.
    WcSessionHandle *h = (__bridge_transfer WcSessionHandle *)handle;
    [h.session stopRunning];
}

int wc_capture_frame(void *handle, uint8_t **out_data, size_t *out_size) {
    if (!handle || !out_data || !out_size) return -1;

    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    NSData *frame = [h.delegate copyLatestFrame];
    if (!frame || frame.length == 0) return -1;

    uint8_t *buf = (uint8_t *)malloc(frame.length);
    if (!buf) return -1;

    memcpy(buf, frame.bytes, frame.length);
    *out_data = buf;
    *out_size = frame.length;
    return 0;
}

void wc_free_frame(uint8_t *data) {
    free(data);
}
