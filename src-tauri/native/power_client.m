#import <Foundation/Foundation.h>
#import <Security/Security.h>
#import <xpc/xpc.h>

// Public XPC peer requirements (macOS 12+) bind both ends to approved code.
char *vela_power_request(const char *json, const char *requirement, uint64_t timeout_ms) {
    @autoreleasepool {
        dispatch_queue_t queue = dispatch_queue_create("com.lich13.vela.power-client", DISPATCH_QUEUE_SERIAL);
        xpc_connection_t connection = xpc_connection_create_mach_service("com.lich13.vela.power-helper", queue, XPC_CONNECTION_MACH_SERVICE_PRIVILEGED);
        if (xpc_connection_set_peer_code_signing_requirement(connection, requirement) != 0) return NULL;
        xpc_connection_set_event_handler(connection, ^(xpc_object_t event) { (void)event; });
        xpc_connection_resume(connection);
        xpc_object_t message = xpc_dictionary_create(NULL, NULL, 0);
        xpc_dictionary_set_string(message, "json", json);
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        __block NSString *result = nil;
        xpc_connection_send_message_with_reply(connection, message, queue, ^(xpc_object_t reply) {
            if (xpc_get_type(reply) == XPC_TYPE_DICTIONARY) {
                const char *value = xpc_dictionary_get_string(reply, "json");
                if (value && strnlen(value, 16385) <= 16384) result = @(value);
            }
            dispatch_semaphore_signal(done);
        });
        BOOL completed = dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, timeout_ms * NSEC_PER_MSEC)) == 0;
        __block char *answer = NULL;
        // Serialize with the callback; never let a late reply access freed storage.
        dispatch_sync(queue, ^{ if (completed && result) answer = strdup(result.UTF8String); });
        xpc_connection_cancel(connection);
        return answer;
    }
}
char *vela_power_identity(void) {
    @autoreleasepool {
        SecCodeRef code = NULL;
        CFDictionaryRef info = NULL;
        if (SecCodeCopySelf(kSecCSDefaultFlags, &code) != errSecSuccess) return NULL;
        OSStatus status = SecCodeCopySigningInformation(code, kSecCSSigningInformation, &info);
        CFRelease(code);
        if (status != errSecSuccess) return NULL;
        NSData *hash = ((__bridge NSDictionary *)info)[(__bridge NSString *)kSecCodeInfoUnique];
        NSMutableString *hex = [NSMutableString string];
        for (NSUInteger i = 0; i < hash.length; ++i) [hex appendFormat:@"%02x", ((const unsigned char *)hash.bytes)[i]];
        char *result = hash.length ? strdup(hex.UTF8String) : NULL;
        CFRelease(info);
        return result;
    }
}
void vela_power_free(char *value) { free(value); }
