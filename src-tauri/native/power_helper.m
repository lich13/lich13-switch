//! Narrow, root-owned power service. No arbitrary command/path is accepted over XPC.
#import <Foundation/Foundation.h>
#import <Security/Security.h>
#import <xpc/xpc.h>
#include <sys/stat.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/file.h>

static NSString *const Label = @"com.lich13.gpt-switch.power-helper";
static NSString *const Binary = @"/Library/PrivilegedHelperTools/com.lich13.gpt-switch.power-helper";
static NSString *const Plist = @"/Library/LaunchDaemons/com.lich13.gpt-switch.power-helper.plist";
static NSString *const Root = @"/Library/Application Support/gpt-Switch Power";
static NSString *const Manifest = @"/Library/Application Support/gpt-Switch Power/client.json";
static NSString *const Restore = @"/Library/Application Support/gpt-Switch Power/restore.json";
static NSString *const InstallReady = @"/Library/Application Support/gpt-Switch Power/install-ready.json";
static NSString *const AppBinary = @"/Applications/lich13-switch.app/Contents/MacOS/lich13-switch";
static NSDictionary *Approved;
#ifdef POWER_TEST
static NSDictionary *TestState;
static BOOL TestFailWrite;
static NSUInteger TestFieldWrites, TestFailFieldWrite, TestExternalAfterFieldWrite, TestFailRecordAfterFieldWrite;
static NSDictionary *TestExternalState;
static NSMutableDictionary *TestFiles;
#endif
static NSDictionary *failure(NSString *code, NSString *message) {
    return @{@"error":@{@"code":code,@"message":message}};
}
static NSData *jsonData(id obj) { return [NSJSONSerialization dataWithJSONObject:obj options:0 error:nil]; }
static NSDictionary *readJSON(NSString *path) {
#ifdef POWER_TEST
    return TestFiles[path];
#else
    NSData *data = [NSData dataWithContentsOfFile:path];
    if (!data || data.length > 16384) return nil;
    id value = [NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    return [value isKindOfClass:NSDictionary.class] ? value : nil;
#endif
}
static BOOL safeRootMetadata(uid_t uid, mode_t mode, BOOL directory) {
    return uid==0 && !(mode&0022) && (directory ? S_ISDIR(mode) : S_ISREG(mode));
}
static BOOL safeRootPath(NSString *path, BOOL directory) {
    struct stat st;
    return lstat(path.fileSystemRepresentation,&st)==0 && safeRootMetadata(st.st_uid,st.st_mode,directory);
}
static BOOL atomicData(NSString *path, NSData *data, mode_t mode) {
    NSString *tmp = [path stringByAppendingFormat:@".%@", NSUUID.UUID.UUIDString];
    int fd = open(tmp.fileSystemRepresentation, O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW, mode);
    if(fd<0) return NO;
    const char *bytes=data.bytes; size_t left=data.length;
    BOOL ok=YES;
    while(left){ssize_t n=write(fd,bytes,left); if(n<0&&errno==EINTR)continue; if(n<=0){ok=NO;break;} bytes+=n;left-=(size_t)n;}
    if(fchmod(fd,mode)||fsync(fd))ok=NO;
    close(fd);
    if(ok)ok=rename(tmp.fileSystemRepresentation,path.fileSystemRepresentation)==0;
    if(!ok)unlink(tmp.fileSystemRepresentation);
    return ok;
}
static BOOL writeJSON(NSString *path, NSDictionary *data) {
#ifdef POWER_TEST
    if(TestFailWrite)return NO;
    if(data)TestFiles[path]=data;else[TestFiles removeObjectForKey:path];
    return YES;
#else
    if(!data)return unlink(path.fileSystemRepresentation)==0||errno==ENOENT;
    return atomicData(path,jsonData(data),0600);
#endif
}
static BOOL run(NSString *binary, NSArray<NSString *> *args, NSString **output) {
    NSTask *task=[NSTask new];task.executableURL=[NSURL fileURLWithPath:binary];task.arguments=args;
    task.environment=@{@"PATH":@"/usr/bin:/bin:/usr/sbin:/sbin",@"LANG":@"C"};
    NSPipe *pipe=[NSPipe pipe]; task.standardOutput=pipe;task.standardError=NSFileHandle.fileHandleWithNullDevice;
    dispatch_semaphore_t done=dispatch_semaphore_create(0);
    task.terminationHandler=^(NSTask *finished){dispatch_semaphore_signal(done);};
    NSError *error=nil;
    if(![task launchAndReturnError:&error])return NO;
    if(dispatch_semaphore_wait(done,dispatch_time(DISPATCH_TIME_NOW,3*NSEC_PER_SEC))!=0){
        if(task.running)kill(task.processIdentifier,SIGKILL);
        dispatch_semaphore_wait(done,dispatch_time(DISPATCH_TIME_NOW,NSEC_PER_SEC));
        return NO;
    }
    NSData *data=[pipe.fileHandleForReading readDataToEndOfFile];
    if(output)*output=[[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding];
    return task.terminationStatus==0;
}
static NSDictionary *powerState(void) {
#ifdef POWER_TEST
    return TestState;
#else
    NSString *general=nil,*custom=nil;
    if(!run(@"/usr/bin/pmset",@[@"-g"],&general)||!run(@"/usr/bin/pmset",@[@"-g",@"custom"],&custom))return nil;
    NSNumber *enabled=nil,*sleep=nil;BOOL battery=NO,supported=NO;
    for(NSString *line in [general componentsSeparatedByString:@"\n"]){
        NSArray *w=[[line componentsSeparatedByCharactersInSet:NSCharacterSet.whitespaceCharacterSet] filteredArrayUsingPredicate:[NSPredicate predicateWithFormat:@"length > 0"]];
        if(w.count>=2&&[w[0] isEqual:@"SleepDisabled"]&&([w[1] isEqual:@"0"]||[w[1] isEqual:@"1"]))enabled=@([w[1] boolValue]);
    }
    for(NSString *line in [custom componentsSeparatedByString:@"\n"]){
        if([line isEqual:@"Battery Power:"]){battery=YES;supported=YES;continue;}
        if(line.length&&![[NSCharacterSet whitespaceCharacterSet] characterIsMember:[line characterAtIndex:0]])battery=NO;
        NSArray *w=[[line componentsSeparatedByCharactersInSet:NSCharacterSet.whitespaceCharacterSet] filteredArrayUsingPredicate:[NSPredicate predicateWithFormat:@"length > 0"]];
        if(battery&&w.count>=2&&[w[0] isEqual:@"sleep"]){NSScanner *scan=[NSScanner scannerWithString:w[1]];NSInteger n=-1;if([scan scanInteger:&n]&&scan.isAtEnd&&n>=0&&n<=1440)sleep=@(n);}
    }
    if(!supported)return @{@"supported":@NO,@"enabled":@NO,@"batterySleep":@0};
    if(!enabled||!sleep)return nil;
    return @{@"supported":@YES,@"enabled":enabled,@"batterySleep":sleep};
#endif
}
static BOOL number(id value, NSInteger max) {
    if(![value isKindOfClass:NSNumber.class])return NO;
    double d=[value doubleValue];return isfinite(d)&&d>=0&&d<=max&&floor(d)==d;
}
static BOOL same(NSDictionary *a, NSDictionary *b) {
    return a&&b&&[a[@"enabled"] isEqual:b[@"enabled"]]&&[a[@"batterySleep"] isEqual:b[@"batterySleep"]];
}
// Restore authority is root-owned. Version 1 records came from this same helper;
// user-level/script records have no verifiable ownership and are never applied.
static NSDictionary *restoreRecord(void) {
    NSDictionary *r=readJSON(Restore);
    if(!r)return nil;
    if([r[@"version"] isEqual:@1] && number(r[@"minutes"],1440) &&
       [r[@"expected"] isKindOfClass:NSDictionary.class]) {
        return @{@"version":@2,@"original":@{@"enabled":@NO,@"batterySleep":r[@"minutes"]},
                 @"expected":r[@"expected"],@"owned":@[@"enabled",@"batterySleep"],@"transaction":@"legacy-helper"};
    }
    return r;
}
static BOOL validRecord(NSDictionary *r) {
    return !r || ([r[@"version"] isEqual:@2] &&
      [r[@"original"] isKindOfClass:NSDictionary.class] &&
      [r[@"expected"] isKindOfClass:NSDictionary.class] &&
      number(r[@"original"][@"enabled"],1) && number(r[@"original"][@"batterySleep"],1440) &&
      number(r[@"expected"][@"enabled"],1) && number(r[@"expected"][@"batterySleep"],1440));
}
static BOOL owns(NSDictionary *record, NSDictionary *actual, NSString *key) {
    return record && ![record[@"release"] boolValue] && [record[@"owned"] containsObject:key] && ![record[@"original"][key] isEqual:record[@"expected"][key]] &&
      [actual[key] isEqual:record[@"expected"][key]];
}
static NSDictionary *stateView(NSDictionary *state, NSDictionary *record) {
    NSMutableDictionary *view=[state mutableCopy];
    BOOL external=record&&!same(state,record[@"expected"]);
    BOOL owned=owns(record,state,@"enabled")||owns(record,state,@"batterySleep");
    view[@"ownership"]=owned?(external?@"mixed":@"application"):([state[@"enabled"] boolValue]?@"external":@"none");
    view[@"externalChanged"]=@(external);
    view[@"transaction"]=record[@"transaction"]?:@"";
    return @{@"state":view,@"version":@1};
}
static BOOL writeField(NSString *key, NSNumber *value) {
#ifdef POWER_TEST
    TestFieldWrites++;
    BOOL ok=TestFieldWrites!=TestFailFieldWrite;
    if(ok){NSMutableDictionary *next=[TestState mutableCopy];next[key]=value;TestState=next;}
    if(TestFieldWrites==TestExternalAfterFieldWrite)TestState=TestExternalState;
    if(TestFieldWrites==TestFailRecordAfterFieldWrite)TestFailWrite=YES;
    return ok;
#else
    return run(@"/usr/bin/pmset",@[@"-b",[key isEqual:@"enabled"]?@"disablesleep":@"sleep",value.stringValue],NULL);
#endif
}
// Recheck the whole observed state immediately before each field write. Rollback
// only touches a field while it still equals this transaction's write.
static BOOL applyOwned(NSDictionary *before, NSDictionary *desired, NSMutableDictionary *written) {
    NSMutableDictionary *expected=[before mutableCopy];
    NSArray *keys=[desired[@"enabled"] boolValue]?@[@"batterySleep",@"enabled"]:@[@"enabled",@"batterySleep"];
    for(NSString *key in keys){
        if([before[key] isEqual:desired[key]])continue;
        if(!same(powerState(),expected))return NO;
        if(!writeField(key,desired[key]))return NO;
        written[key]=desired[key];expected[key]=desired[key];
        NSMutableDictionary *record=[readJSON(Restore) mutableCopy];
        if(record){record[@"owned"]=written.allKeys;if(!writeJSON(Restore,record))return NO;}
    }
    return same(powerState(),desired);
}
static BOOL rollbackOwned(NSDictionary *before, NSDictionary *written) {
    BOOL ok=YES;
    for(NSString *key in @[@"enabled",@"batterySleep"]){
        NSDictionary *now=powerState();
        if(written[key] && [now[key] isEqual:written[key]])ok=writeField(key,before[key])&&ok;
    }
    return ok;
}
static NSDictionary *handle(NSDictionary *request) {
    NSString *op=request[@"op"];
    NSDictionary *before=powerState();
    if(!before)return failure(@"POWER",@"无法读取系统电源状态");
    NSDictionary *record=restoreRecord();
#ifndef POWER_TEST
    if([[NSFileManager defaultManager] fileExistsAtPath:Restore]&&!record)return failure(@"POWER",@"电源恢复记录无效");
#endif
    if(!validRecord(record))return failure(@"POWER",@"电源恢复记录无效");
    if([op isEqual:@"state"])return stateView(before,record);
    if([op isEqual:@"verifyInstall"]){
        NSString *transaction=readJSON(Manifest)[@"transaction"];
        if(![transaction isKindOfClass:NSString.class]||![request[@"transaction"] isEqual:transaction])
            return failure(@"POWER_INSTALL_VERIFY",@"没有匹配的助手安装事务");
        if(!writeJSON(InstallReady,@{@"transaction":transaction}))
            return failure(@"POWER_INSTALL_VERIFY",@"无法确认助手连接");
        return stateView(before,record);
    }
    BOOL removing=[op isEqual:@"prepareRemove"];
    if(![op isEqual:@"set"]&&!removing)return failure(@"POWER_PROTOCOL",@"不支持的电源操作");
    if(!removing){
        if(!number(request[@"enabled"],1)||!number(request[@"beforeEnabled"],1)||!number(request[@"beforeSleep"],1440))
            return failure(@"POWER_PROTOCOL",@"电源参数无效");
        if(![before[@"supported"] boolValue])return failure(@"UNSUPPORTED",@"此设备不支持电池合盖控制");
        if(!same(before,@{@"enabled":request[@"beforeEnabled"],@"batterySleep":request[@"beforeSleep"]}))
            return failure(@"CONFLICT",@"电源状态已变化，请重新操作");
    }
    NSMutableDictionary *desired=[before mutableCopy];
    NSDictionary *nextRecord=nil;
    if(!removing && [request[@"enabled"] boolValue]){
        desired[@"enabled"]=@YES;desired[@"batterySleep"]=@0;
        nextRecord=@{@"version":@2,@"original":before,@"expected":desired,@"owned":@[],@"transaction":NSUUID.UUID.UUIDString};
    }else{
        for(NSString *key in @[@"enabled",@"batterySleep"])
            if(owns(record,before,key))desired[key]=record[@"original"][key];
        // A user explicitly closing an externally enabled state clears only the
        // inhibit flag. Removal itself never clears another application's flag.
        if(!removing)desired[@"enabled"]=@NO;
    }
    // Persist before pmset. Even an interrupted operation retains its before/after
    // values; subsequent explicit operations can safely retire stale ownership.
    NSDictionary *transaction=nextRecord?:@{@"version":@2,@"release":@YES,@"original":before,@"expected":desired,@"transaction":NSUUID.UUID.UUIDString};
    if(!writeJSON(Restore,transaction))return failure(@"POWER",@"无法保存电源恢复记录");
    NSMutableDictionary *written=[NSMutableDictionary dictionary];
    if(!applyOwned(before,desired,written)){
        BOOL rolledBack=rollbackOwned(before,written);
        if(rolledBack&&same(powerState(),before))writeJSON(Restore,record);
        else {
            NSMutableDictionary *remaining=[transaction mutableCopy];NSMutableArray *keys=[NSMutableArray array];
            NSDictionary *actual=powerState();
            for(NSString *key in written)if([actual[key] isEqual:written[key]] && ![actual[key] isEqual:before[key]])[keys addObject:key];
            remaining[@"owned"]=keys;writeJSON(Restore,remaining);
        }
        return failure(@"CONFLICT",@"操作期间电源状态变化，已保留外部修改，请重试");
    }
    if(nextRecord){NSMutableDictionary *claimed=[nextRecord mutableCopy];claimed[@"owned"]=written.allKeys;nextRecord=claimed;}
    if(!writeJSON(Restore,nextRecord)){
        rollbackOwned(before,written);
        return failure(@"POWER",@"电源恢复记录更新失败");
    }
    return stateView(powerState(),nextRecord);
}
static NSString *codePath(SecCodeRef code) {
    CFDictionaryRef info=NULL;if(SecCodeCopySigningInformation(code,kSecCSSigningInformation,&info)!=errSecSuccess)return nil;
    NSURL *url=((__bridge NSDictionary *)info)[(__bridge NSString *)kSecCodeInfoMainExecutable];
    NSString *path=url.path;CFRelease(info);return path;
}
static BOOL identityMatches(uid_t uid, NSString *path, BOOL signatureValid, NSDictionary *approved) {
    return signatureValid && uid==[approved[@"uid"] unsignedIntValue] && [path isEqual:approved[@"path"]];
}
static BOOL authorized(xpc_connection_t peer, xpc_object_t message) {
    if(xpc_connection_get_euid(peer)!=[Approved[@"uid"] unsignedIntValue])return NO;
    SecCodeRef code=NULL;SecRequirementRef req=NULL;
    if(SecCodeCreateWithXPCMessage(message,kSecCSDefaultFlags,&code)!=errSecSuccess)return NO;
    OSStatus status=SecRequirementCreateWithString((__bridge CFStringRef)Approved[@"requirement"],kSecCSDefaultFlags,&req);
    BOOL ok=identityMatches(xpc_connection_get_euid(peer),codePath(code),status==errSecSuccess&&SecCodeCheckValidity(code,kSecCSStrictValidate,req)==errSecSuccess,Approved);
    if(req)CFRelease(req);CFRelease(code);return ok;
}
static BOOL safeDirectory(NSString *path) {
    if(mkdir(path.fileSystemRepresentation,0700)!=0&&errno!=EEXIST)return NO;
    return safeRootPath(path,YES);
}
static BOOL restoreFile(NSString *path, NSData *data, mode_t mode) {
    return data ? atomicData(path,data,mode) : unlink(path.fileSystemRepresentation)==0||errno==ENOENT;
}
#include "power_install.h"
#ifndef POWER_TEST
int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if(argc==6&&strcmp(argv[1],"--install")==0){
            char *end=NULL;unsigned long uid=strtoul(argv[2],&end,10);
            NSDictionary *result=end&&!*end&&uid<=UINT32_MAX ? install((uid_t)uid,@(argv[3]),@(argv[4]),@(argv[5])) : installFailure(@"SIGNATURE",@"安装参数无效",@"unchanged");
            puts([[NSString alloc] initWithData:jsonData(result) encoding:NSUTF8StringEncoding].UTF8String);
            return 0;
        }
        if(argc!=1||geteuid()!=0||!safeRootPath(Root,YES)||!safeRootPath(Manifest,NO))return 1;
        Approved=readJSON(Manifest);
        if(!number(Approved[@"uid"],UINT32_MAX)||![Approved[@"path"] isEqual:AppBinary]||![Approved[@"requirement"] isKindOfClass:NSString.class])return 1;
        dispatch_queue_t queue=dispatch_queue_create("com.lich13.gpt-switch.power",DISPATCH_QUEUE_SERIAL);
        xpc_connection_t service=xpc_connection_create_mach_service(Label.UTF8String,queue,XPC_CONNECTION_MACH_SERVICE_LISTENER);
        xpc_connection_set_event_handler(service,^(xpc_object_t event){
            if(xpc_get_type(event)!=XPC_TYPE_CONNECTION)return;
            xpc_connection_t peer=(xpc_connection_t)event;
            if(xpc_connection_set_peer_code_signing_requirement(peer,[Approved[@"requirement"] UTF8String])!=0){xpc_connection_cancel(peer);return;}
            xpc_connection_set_target_queue(peer,queue);
            xpc_connection_set_event_handler(peer,^(xpc_object_t message){
                if(xpc_get_type(message)!=XPC_TYPE_DICTIONARY)return;
                xpc_object_t reply=xpc_dictionary_create_reply(message);if(!reply)return;
                NSDictionary *result=nil;BOOL remove=NO;
                if(!authorized(peer,message))result=failure(@"POWER_AUTH",@"应用版本未授权，请修复电源助手");
                else {
                    const char *raw=xpc_dictionary_get_string(message,"json");
                    if(raw&&strnlen(raw,4097)<=4096){
                        id request=[NSJSONSerialization JSONObjectWithData:[@(raw) dataUsingEncoding:NSUTF8StringEncoding] options:0 error:nil];
                        if([request isKindOfClass:NSDictionary.class]){
                            remove=[request[@"op"] isEqual:@"remove"];
                            result=handle(remove?@{@"op":@"prepareRemove"}:request);
                        }
                    }
                    if(!result)result=failure(@"POWER_PROTOCOL",@"电源请求无效");
                }
                if(remove&&!result[@"error"]){
                    // Remove only this service's fixed paths; no caller-supplied path.
                    NSData *manifest=[NSData dataWithContentsOfFile:Manifest],*plist=[NSData dataWithContentsOfFile:Plist],*binary=[NSData dataWithContentsOfFile:Binary];
                    if(!manifest||!plist||!binary)result=failure(@"POWER",@"无法读取电源助手注册文件");
                    else {
                        BOOL ok=unlink(Plist.fileSystemRepresentation)==0&&unlink(Binary.fileSystemRepresentation)==0&&unlink(Manifest.fileSystemRepresentation)==0;
                        if(!ok){restoreFile(Manifest,manifest,0600);restoreFile(Plist,plist,0644);restoreFile(Binary,binary,0755);result=failure(@"POWER",@"电源助手移除失败，原注册文件已尝试恢复");}
                        else {
                            unlink([[Root stringByAppendingPathComponent:@"install.lock"] fileSystemRepresentation]);
                            unlink(InstallReady.fileSystemRepresentation);
                            rmdir(Root.fileSystemRepresentation);
                            dispatch_after(dispatch_time(DISPATCH_TIME_NOW,NSEC_PER_SEC),queue,^{run(@"/bin/launchctl",@[@"bootout",[@"system/" stringByAppendingString:Label]],NULL);});
                        }
                    }
                }
                xpc_dictionary_set_string(reply,"json",[[[NSString alloc] initWithData:jsonData(result) encoding:NSUTF8StringEncoding] UTF8String]);
                xpc_connection_send_message(peer,reply);
            });
            xpc_connection_resume(peer);
        });
        xpc_connection_resume(service);dispatch_main();
    }
}
#else
#define CHECK(v) do { if(!(v)){fprintf(stderr,"power test failed at %d\n",__LINE__);return 1;} } while(0)
static NSDictionary *testState(BOOL enabled, NSInteger minutes) {
    return @{@"supported":@YES,@"enabled":@(enabled),@"batterySleep":@(minutes)};
}
static void testReset(BOOL enabled, NSInteger minutes) {
    TestFiles=[NSMutableDictionary dictionary];TestState=testState(enabled,minutes);
    TestFailWrite=NO;TestFieldWrites=0;TestFailFieldWrite=0;
    TestExternalAfterFieldWrite=0;TestExternalState=nil;TestFailRecordAfterFieldWrite=0;
}
static NSDictionary *testSet(BOOL enabled) {
    // An old caller's default must never be used as restoration authority.
    return handle(@{@"op":@"set",@"enabled":@(enabled),@"minutes":@1,
                    @"beforeEnabled":TestState[@"enabled"],@"beforeSleep":TestState[@"batterySleep"]});
}
static BOOL testView(NSString *ownership, BOOL externalChanged) {
    NSDictionary *view=handle(@{@"op":@"state"})[@"state"];
    return [view[@"ownership"] isEqual:ownership]&&[view[@"externalChanged"] isEqual:@(externalChanged)];
}
int main(void) {
    @autoreleasepool {
        CHECK(testInstallTransactions());
        NSDictionary *approved=@{@"uid":@501,@"path":AppBinary};
        CHECK(identityMatches(501,AppBinary,YES,approved));
        CHECK(!identityMatches(502,AppBinary,YES,approved));
        CHECK(!identityMatches(501,@"/tmp/lich13-switch",YES,approved));
        CHECK(!identityMatches(501,AppBinary,NO,approved));
        testReset(NO,0);
        TestFiles[Manifest]=@{@"transaction":@"fresh-install"};
        CHECK(handle(@{@"op":@"verifyInstall",@"transaction":@"old-install"})[@"error"]);
        CHECK(!readJSON(InstallReady));
        CHECK(!handle(@{@"op":@"verifyInstall",@"transaction":@"fresh-install"})[@"error"]);
        CHECK([readJSON(InstallReady)[@"transaction"] isEqual:@"fresh-install"]);
        CHECK([TestState[@"enabled"] isEqual:@NO]);
        [TestFiles removeObjectForKey:Manifest];[TestFiles removeObjectForKey:InstallReady];
        CHECK(handle(@{@"op":@"verifyInstall",@"transaction":@"fresh-install"})[@"error"]);

        // A real zero remains zero, while a nonzero original is owned by root.
        for(NSNumber *minutes in @[@0,@7]){
            testReset(NO,minutes.integerValue);
            CHECK(testView(@"none",NO));
            CHECK(!testSet(YES)[@"error"]);
            CHECK(same(TestState,testState(YES,0)));
            CHECK([readJSON(Restore)[@"version"] isEqual:@2]);
            CHECK([readJSON(Restore)[@"original"][@"batterySleep"] isEqual:minutes]);
            CHECK([readJSON(Restore)[@"expected"][@"batterySleep"] isEqual:@0]);
            CHECK(testView(@"application",NO));
            if(minutes.integerValue==0)CHECK(TestFieldWrites==1);
            CHECK(!testSet(NO)[@"error"]);
            CHECK(same(TestState,testState(NO,minutes.integerValue)));
            CHECK(!readJSON(Restore));CHECK(testView(@"none",NO));
        }

        // External enable has no restoration record; closing only clears its flag.
        for(NSNumber *minutes in @[@0,@5]){
            testReset(YES,minutes.integerValue);
            CHECK(testView(@"external",NO));
            CHECK(!testSet(NO)[@"error"]);
            CHECK(same(TestState,testState(NO,minutes.integerValue)));
            CHECK(TestFieldWrites==1);CHECK(!readJSON(Restore));
            testReset(YES,minutes.integerValue);
            CHECK(!handle(@{@"op":@"prepareRemove"})[@"error"]);
            CHECK(same(TestState,testState(YES,minutes.integerValue)));
            CHECK(TestFieldWrites==0);CHECK(testView(@"external",NO));
        }

        // Taking over an already-enabled state owns sleep only; removal keeps its flag.
        testReset(YES,5);
        CHECK(!testSet(YES)[@"error"]);
        CHECK(!owns(restoreRecord(),TestState,@"enabled"));
        CHECK(owns(restoreRecord(),TestState,@"batterySleep"));
        CHECK(!handle(@{@"op":@"prepareRemove"})[@"error"]);
        CHECK(same(TestState,testState(YES,5)));CHECK(!readJSON(Restore));

        // External sleep changes survive both explicit close and helper removal.
        for(NSString *op in @[@"set",@"prepareRemove"]){
            testReset(NO,7);CHECK(!testSet(YES)[@"error"]);
            TestState=testState(YES,3);CHECK(testView(@"mixed",YES));
            NSDictionary *result=[op isEqual:@"set"]?testSet(NO):handle(@{@"op":op});
            CHECK(!result[@"error"]);CHECK(same(TestState,testState(NO,3)));
            CHECK(!readJSON(Restore));
        }

        // External close releases enabled ownership but leaves untouched sleep restorable.
        testReset(NO,7);CHECK(!testSet(YES)[@"error"]);
        TestState=testState(NO,0);CHECK(testView(@"mixed",YES));
        CHECK(!testSet(NO)[@"error"]);CHECK(same(TestState,testState(NO,7)));
        CHECK(!readJSON(Restore));

        // An expired record is rebased at the next explicit enable, including zero.
        for(NSNumber *minutes in @[@0,@4]){
            testReset(NO,7);CHECK(!testSet(YES)[@"error"]);
            NSString *previousTransaction=readJSON(Restore)[@"transaction"];
            TestState=testState(NO,minutes.integerValue);
            CHECK(!testSet(YES)[@"error"]);
            CHECK(![readJSON(Restore)[@"transaction"] isEqual:previousTransaction]);
            CHECK(same(readJSON(Restore)[@"original"],testState(NO,minutes.integerValue)));
            CHECK(!testSet(NO)[@"error"]);
            CHECK(same(TestState,testState(NO,minutes.integerValue)));CHECK(!readJSON(Restore));
        }
        testReset(NO,7);CHECK(!testSet(YES)[@"error"]);
        TestState=testState(NO,4);CHECK(testView(@"none",YES));
        NSUInteger writes=TestFieldWrites;
        CHECK(!handle(@{@"op":@"prepareRemove"})[@"error"]);
        CHECK(same(TestState,testState(NO,4)));CHECK(TestFieldWrites==writes);
        CHECK(!readJSON(Restore));

        // Root v1 is trusted and normalized to v2 without inventing a default.
        for(NSNumber *minutes in @[@0,@7]){
            testReset(YES,0);
            TestFiles[Restore]=@{@"version":@1,@"minutes":minutes,@"expected":testState(YES,0)};
            CHECK([restoreRecord()[@"version"] isEqual:@2]);
            CHECK(same(restoreRecord()[@"original"],testState(NO,minutes.integerValue)));
            CHECK(testView(@"application",NO));
            CHECK(!testSet(NO)[@"error"]);
            CHECK(same(TestState,testState(NO,minutes.integerValue)));CHECK(!readJSON(Restore));
        }

        // Interrupted release transactions never acquire inverse ownership.
        testReset(NO,7);
        TestFiles[Restore]=@{@"version":@2,@"original":testState(YES,0),@"expected":testState(NO,7),
                             @"owned":@[@"enabled",@"batterySleep"],@"transaction":@"interrupted-release",@"release":@YES};
        CHECK(testView(@"none",NO));
        CHECK(!handle(@{@"op":@"prepareRemove"})[@"error"]);
        CHECK(same(TestState,testState(NO,7)));CHECK(TestFieldWrites==0);

        // CAS rejects an outdated request before persisting or writing a field.
        testReset(NO,7);
        CHECK([handle(@{@"op":@"set",@"enabled":@YES,@"beforeEnabled":@NO,@"beforeSleep":@1})[@"error"][@"code"] isEqual:@"CONFLICT"]);
        CHECK(same(TestState,testState(NO,7)));CHECK(TestFieldWrites==0);CHECK(!readJSON(Restore));
        CHECK([handle(@{@"op":@"run",@"command":@"true"})[@"error"][@"code"] isEqual:@"POWER_PROTOCOL"]);
        CHECK([handle(@{@"op":@"set",@"enabled":@2,@"beforeEnabled":@NO,@"beforeSleep":@7})[@"error"][@"code"] isEqual:@"POWER_PROTOCOL"]);

        // A second-field failure rolls back only the first write.
        testReset(NO,7);TestFailFieldWrite=2;
        CHECK([testSet(YES)[@"error"][@"code"] isEqual:@"CONFLICT"]);
        CHECK(same(TestState,testState(NO,7)));CHECK(TestFieldWrites==3);CHECK(!readJSON(Restore));

        // CAS between writes preserves an external sleep value through rollback.
        testReset(NO,7);TestExternalAfterFieldWrite=1;TestExternalState=testState(NO,4);
        CHECK([testSet(YES)[@"error"][@"code"] isEqual:@"CONFLICT"]);
        CHECK(same(TestState,testState(NO,4)));CHECK(TestFieldWrites==1);
        CHECK(!testSet(YES)[@"error"]);CHECK(!testSet(NO)[@"error"]);
        CHECK(same(TestState,testState(NO,4)));

        // A simultaneous external enable must not be claimed by a failed transaction.
        testReset(NO,7);TestExternalAfterFieldWrite=1;TestExternalState=testState(YES,0);
        CHECK([testSet(YES)[@"error"][@"code"] isEqual:@"CONFLICT"]);
        CHECK(same(TestState,testState(YES,7)));
        CHECK(!handle(@{@"op":@"prepareRemove"})[@"error"]);
        CHECK(same(TestState,testState(YES,7)));

        // Field-conditional rollback also protects a value changed after a write.
        testReset(YES,4);
        CHECK(rollbackOwned(testState(NO,7),@{@"enabled":@YES,@"batterySleep":@0}));
        CHECK(same(TestState,testState(NO,4)));CHECK(TestFieldWrites==1);

        // Persistence failures never make a field write before the root record exists.
        testReset(NO,7);TestFailWrite=YES;
        CHECK([testSet(YES)[@"error"][@"code"] isEqual:@"POWER"]);
        CHECK(same(TestState,testState(NO,7)));CHECK(TestFieldWrites==0);CHECK(!readJSON(Restore));
        TestFailWrite=NO;CHECK(!testSet(YES)[@"error"]);
        TestFailWrite=YES;writes=TestFieldWrites;
        CHECK(handle(@{@"op":@"prepareRemove"})[@"error"]);
        CHECK(testSet(NO)[@"error"]);CHECK(same(TestState,testState(YES,0)));
        CHECK(TestFieldWrites==writes);CHECK(readJSON(Restore));
        TestFailWrite=NO;CHECK(!handle(@{@"op":@"prepareRemove"})[@"error"]);
        CHECK(same(TestState,testState(NO,7)));CHECK(!readJSON(Restore));

        // A record update failure after the second field rolls back both writes.
        testReset(NO,7);TestFailRecordAfterFieldWrite=2;
        CHECK([testSet(YES)[@"error"][@"code"] isEqual:@"CONFLICT"]);
        CHECK(same(TestState,testState(NO,7)));CHECK(TestFieldWrites==4);
        TestFailWrite=NO;CHECK(!handle(@{@"op":@"prepareRemove"})[@"error"]);
        CHECK(same(TestState,testState(NO,7)));

        testReset(NO,7);TestFiles[Restore]=@{@"version":@2,@"original":@{},@"expected":@{}};
        CHECK([testSet(YES)[@"error"][@"code"] isEqual:@"POWER"]);
        CHECK(same(TestState,testState(NO,7)));CHECK(TestFieldWrites==0);
        puts("power helper transaction tests passed (simulated)");
    }return 0;
}
#endif
