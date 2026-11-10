// Installation commits only after the approved app completes a real XPC round trip.
// Backups remain in this privileged process's memory and are discarded on success.
static NSDictionary *installFailure(NSString *stage, NSString *message, NSString *rollback) {
    NSString *suffix=[rollback isEqual:@"restored"] ? @"原注册状态已恢复" :
        [rollback isEqual:@"failed"] ? @"原注册状态恢复失败，请重新修复助手" : @"未更改注册状态";
    return @{@"error":@{@"code":[@"POWER_INSTALL_" stringByAppendingString:stage],
        @"message":[NSString stringWithFormat:@"%@；%@",message,suffix]},@"rollback":rollback};
}
#ifdef POWER_TEST
static NSMutableDictionary *TestInstallFiles;
static BOOL TestLoaded, TestVerify, TestRollingBack, TestRollbackFailure, TestStopFailure;
static NSInteger TestWriteFailure, TestWrites, TestStartFailure;
#endif
static NSDictionary *installSnapshot(NSString *path) {
#ifdef POWER_TEST
    return TestInstallFiles[path] ?: @{@"exists":@NO};
#else
    struct stat st;
    if(lstat(path.fileSystemRepresentation,&st)!=0)return errno==ENOENT ? @{@"exists":@NO} : nil;
    if(!safeRootMetadata(st.st_uid,st.st_mode,NO))return nil;
    NSData *data=[NSData dataWithContentsOfFile:path];
    return data ? @{@"exists":@YES,@"data":data,@"mode":@(st.st_mode&07777),@"gid":@(st.st_gid)} : nil;
#endif
}
static BOOL installWrite(NSString *path, NSDictionary *snapshot) {
#ifdef POWER_TEST
    if(TestRollingBack&&TestRollbackFailure)return NO;
    if(!TestRollingBack&&++TestWrites==TestWriteFailure)return NO;
    if([snapshot[@"exists"] boolValue])TestInstallFiles[path]=snapshot;
    else[TestInstallFiles removeObjectForKey:path];
    return YES;
#else
    if(![snapshot[@"exists"] boolValue])return unlink(path.fileSystemRepresentation)==0||errno==ENOENT;
    mode_t mode=[snapshot[@"mode"] unsignedShortValue];
    return atomicData(path,snapshot[@"data"],mode)&&
        chown(path.fileSystemRepresentation,0,[snapshot[@"gid"] unsignedIntValue])==0&&
        chmod(path.fileSystemRepresentation,mode)==0;
#endif
}
static NSDictionary *installFile(NSData *data, mode_t mode) {
    return @{@"exists":@YES,@"data":data,@"mode":@(mode),@"gid":@0};
}
static BOOL serviceLoaded(void) {
#ifdef POWER_TEST
    return TestLoaded;
#else
    return run(@"/bin/launchctl",@[@"print",[@"system/" stringByAppendingString:Label]],NULL);
#endif
}
static BOOL serviceSet(BOOL loaded) {
#ifdef POWER_TEST
    if(TestRollingBack&&TestRollbackFailure)return NO;
    if(!loaded&&TestStopFailure)return NO;
    if(loaded&&TestStartFailure>0){TestStartFailure--;return NO;}
    TestLoaded=loaded;return YES;
#else
    run(@"/bin/launchctl",loaded ? @[@"bootstrap",@"system",Plist] : @[@"bootout",[@"system/" stringByAppendingString:Label]],NULL);
    return serviceLoaded()==loaded;
#endif
}
static BOOL installVerified(NSString *transaction) {
#ifdef POWER_TEST
    return TestVerify;
#else
    // No power setting is changed. The app must prove that the new service accepts
    // its current code signature and returns a valid power state before commit.
    for(int i=0;i<300;i++){
        if([readJSON(InstallReady)[@"transaction"] isEqual:transaction])return YES;
        usleep(100000);
    }
    return NO;
#endif
}
static NSDictionary *installTransaction(NSData *binary, NSData *plist, NSDictionary *client) {
    NSArray *paths=@[Binary,Plist,Manifest];
    NSMutableDictionary *old=[NSMutableDictionary dictionary];
    for(NSString *path in paths){NSDictionary *snapshot=installSnapshot(path);if(!snapshot)return installFailure(@"READ",@"无法读取原助手注册文件",@"unchanged");old[path]=snapshot;}
    BOOL loaded=serviceLoaded();
    if(loaded&&!serviceSet(NO))return installFailure(@"REGISTER",@"无法停止原电源助手",@"unchanged");
    NSDictionary *desired=@{Binary:installFile(binary,0755),Plist:installFile(plist,0644),Manifest:installFile(jsonData(client),0600)};
    NSString *stage=@"WRITE",*message=@"电源助手文件写入失败";
    BOOL ok=YES;
    for(NSString *path in paths){if(!installWrite(path,desired[path])||![installSnapshot(path) isEqual:desired[path]]){ok=NO;break;}}
    if(ok){stage=@"REGISTER";message=@"电源助手服务注册失败";ok=serviceSet(YES);}
    if(ok){stage=@"CONNECT";message=@"电源助手 XPC 连接校验失败";ok=installVerified(client[@"transaction"]);}
    if(ok){
        NSMutableDictionary *committed=[client mutableCopy];[committed removeObjectForKey:@"transaction"];
        NSDictionary *file=installFile(jsonData(committed),0600);
        stage=@"WRITE";message=@"电源助手安装确认失败";
        ok=installWrite(Manifest,file)&&[installSnapshot(Manifest) isEqual:file];
    }
    if(ok)return @{@"version":@1};
    BOOL changed=serviceLoaded()!=loaded;
    for(NSString *path in paths)if(![installSnapshot(path) isEqual:old[path]])changed=YES;
    if(!changed)return installFailure(stage,message,@"unchanged");
#ifdef POWER_TEST
    TestRollingBack=YES;
#endif
    BOOL restored=!serviceLoaded()||serviceSet(NO);
    if(restored){
        // Attempt every file even if one fails, then verify bytes and ownership.
        for(NSString *path in paths)if(!installWrite(path,old[path]))restored=NO;
        for(NSString *path in paths)if(![installSnapshot(path) isEqual:old[path]])restored=NO;
        if(restored&&loaded)restored=serviceSet(YES);
        if(serviceLoaded()!=loaded)restored=NO;
    }
    return installFailure(stage,message,restored?@"restored":@"failed");
}
static NSDictionary *install(uid_t uid, NSString *path, NSString *hash, NSString *transaction) {
    if(geteuid()!=0||uid<501||![path isEqual:AppBinary]||hash.length!=40||
       [hash rangeOfCharacterFromSet:[[NSCharacterSet characterSetWithCharactersInString:@"0123456789abcdef"] invertedSet]].location!=NSNotFound||
       ![[NSUUID alloc] initWithUUIDString:transaction])return installFailure(@"SIGNATURE",@"安装参数无效",@"unchanged");
    for(NSString *directory in @[@"/Library/PrivilegedHelperTools",@"/Library/LaunchDaemons",@"/Library/Application Support"])
        if(!safeRootPath(directory,YES))return installFailure(@"DIRECTORY",@"系统助手目录权限不安全",@"unchanged");
    NSString *requirement=[NSString stringWithFormat:@"identifier \"com.lich13.vela\" and cdhash H\"%@\"",hash];
    SecStaticCodeRef code=NULL;SecRequirementRef req=NULL;
    if(SecStaticCodeCreateWithPath((__bridge CFURLRef)[NSURL fileURLWithPath:path],kSecCSDefaultFlags,&code)!=errSecSuccess)
        return installFailure(@"SIGNATURE",@"无法读取应用签名",@"unchanged");
    SecRequirementCreateWithString((__bridge CFStringRef)requirement,kSecCSDefaultFlags,&req);
    BOOL valid=req&&SecStaticCodeCheckValidity(code,kSecCSStrictValidate,req)==errSecSuccess;
    if(req)CFRelease(req);CFRelease(code);
    if(!valid)return installFailure(@"SIGNATURE",@"应用签名验证失败",@"unchanged");
    struct stat st;BOOL existed=lstat(Root.fileSystemRepresentation,&st)==0;
    if(!safeDirectory(Root))return installFailure(@"DIRECTORY",@"电源助手数据目录权限不安全",@"unchanged");
    NSString *lockPath=[Root stringByAppendingPathComponent:@"install.lock"];
    int lock=open(lockPath.fileSystemRepresentation,O_RDWR|O_CREAT|O_NOFOLLOW,0600);
    if(lock<0||fstat(lock,&st)!=0||!safeRootMetadata(st.st_uid,st.st_mode,NO)||flock(lock,LOCK_EX|LOCK_NB)!=0){
        if(lock>=0)close(lock);
        return installFailure(@"BUSY",@"无法锁定安装事务，请稍后重试",@"unchanged");
    }
    NSDictionary *result=nil;
    for(NSString *file in @[Binary,Plist,Manifest,Restore,InstallReady]){
        if(lstat(file.fileSystemRepresentation,&st)==0&&!safeRootPath(file,NO))
            result=installFailure(@"DIRECTORY",@"电源助手文件权限不安全",@"unchanged");
    }
    NSDictionary *oldClient=readJSON(Manifest);
    if(!result&&lstat(Manifest.fileSystemRepresentation,&st)==0&&(!oldClient||[oldClient[@"uid"] unsignedIntValue]!=uid))
        result=installFailure(@"SIGNATURE",@"原电源助手不属于当前用户",@"unchanged");
    if(!result&&!writeJSON(InstallReady,nil))result=installFailure(@"WRITE",@"无法准备安装校验",@"unchanged");
    if(!result){
        NSData *binary=[NSData dataWithContentsOfFile:NSProcessInfo.processInfo.arguments[0]];
        NSDictionary *plist=@{@"Label":Label,@"ProgramArguments":@[Binary],@"MachServices":@{Label:@YES},@"ProcessType":@"Interactive",@"Umask":@077};
        NSData *data=[NSPropertyListSerialization dataWithPropertyList:plist format:NSPropertyListXMLFormat_v1_0 options:0 error:nil];
        result=binary&&data ? installTransaction(binary,data,@{@"uid":@(uid),@"path":path,@"requirement":requirement,@"version":@1,@"transaction":transaction}) : installFailure(@"READ",@"无法读取安装载荷",@"unchanged");
    }
    writeJSON(InstallReady,nil);
    close(lock);
    if(!existed&&result[@"error"]){unlink(lockPath.fileSystemRepresentation);rmdir(Root.fileSystemRepresentation);}
    return result;
}
#ifdef POWER_TEST
static void resetInstallTest(BOOL existing) {
    TestInstallFiles=[NSMutableDictionary dictionary];TestLoaded=existing;TestVerify=YES;
    TestRollingBack=TestRollbackFailure=TestStopFailure=NO;TestWriteFailure=TestWrites=TestStartFailure=0;
    if(existing)for(NSString *p in @[Binary,Plist,Manifest])TestInstallFiles[p]=installFile([@"old" dataUsingEncoding:NSUTF8StringEncoding],0600);
}
static BOOL testInstallTransactions(void) {
    if(!safeRootMetadata(0,S_IFDIR|0755,YES)||!safeRootMetadata(0,S_IFDIR|01755,YES)||
       safeRootMetadata(501,S_IFDIR|0755,YES)||safeRootMetadata(0,S_IFLNK|0755,YES)||
       safeRootMetadata(0,S_IFDIR|0775,YES)||safeRootMetadata(0,S_IFDIR|0757,YES))return NO;
    NSData *payload=[@"new" dataUsingEncoding:NSUTF8StringEncoding];NSDictionary *client=@{@"transaction":@"fixture"};
    resetInstallTest(NO);
    if(installTransaction(payload,payload,client)[@"error"]||!TestLoaded)return NO;
    resetInstallTest(NO);TestWriteFailure=1;
    if(![installTransaction(payload,payload,client)[@"rollback"] isEqual:@"unchanged"]||TestLoaded||TestInstallFiles.count)return NO;
    for(NSNumber *step in @[@1,@2,@3,@4]){
        resetInstallTest(YES);NSDictionary *old=[TestInstallFiles copy];TestWriteFailure=step.integerValue;
        if(![installTransaction(payload,payload,client)[@"rollback"] isEqual:@"restored"]||!TestLoaded||![TestInstallFiles isEqual:old])return NO;
    }
    resetInstallTest(YES);TestStartFailure=1;
    if(![installTransaction(payload,payload,client)[@"rollback"] isEqual:@"restored"]||!TestLoaded)return NO;
    resetInstallTest(NO);TestVerify=NO;
    NSDictionary *error=installTransaction(payload,payload,client);
    if(![error[@"error"][@"code"] isEqual:@"POWER_INSTALL_CONNECT"]||![error[@"rollback"] isEqual:@"restored"]||TestLoaded||TestInstallFiles.count)return NO;
    resetInstallTest(YES);TestVerify=NO;TestRollbackFailure=YES;
    if(![installTransaction(payload,payload,client)[@"rollback"] isEqual:@"failed"])return NO;
    resetInstallTest(YES);TestStopFailure=YES;
    if(![installTransaction(payload,payload,client)[@"rollback"] isEqual:@"unchanged"]||!TestLoaded||TestWrites)return NO;
    return YES;
}
#endif
