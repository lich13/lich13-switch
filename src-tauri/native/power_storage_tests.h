#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static BOOL storageTestDirectoryHasOnly(NSString *directory, NSString *name) {
    NSError *error = nil;
    NSArray<NSString *> *entries = [[NSFileManager defaultManager] contentsOfDirectoryAtPath:directory error:&error];
    if (!entries || error) return NO;
    if (!name) return entries.count == 0;
    return entries.count == 1 && [entries[0] isEqualToString:name];
}

static BOOL storageTestCleanupDirectory(NSString *directory) {
    NSError *error = nil;
    NSArray<NSString *> *entries = [[NSFileManager defaultManager] contentsOfDirectoryAtPath:directory error:&error];
    BOOL ok = entries != nil && error == nil;
    for (NSString *name in entries ?: @[]) {
        NSString *child = [directory stringByAppendingPathComponent:name];
        if (unlink(child.fileSystemRepresentation) != 0 && errno != ENOENT) ok = NO;
    }
    if (rmdir(directory.fileSystemRepresentation) != 0 && errno != ENOENT) ok = NO;
    return ok;
}

static BOOL testStoragePersistence(void) {
    BOOL passed = NO;
    NSString *directory = nil;
    NSString *path = nil;
    NSData *initial = nil;
    NSData *replacement = nil;
    NSData *syncFailureData = nil;
    char template[PATH_MAX] = {0};
    TestFailDirectorySync = NO;

    do {
        NSString *temporary = NSTemporaryDirectory();
        if (temporary.length == 0) break;
        int length = snprintf(template, sizeof(template), "%s/%s",
                              temporary.fileSystemRepresentation, "lich13-power-storage.XXXXXX");
        if (length < 0 || (size_t)length >= sizeof(template)) break;

        char *created = mkdtemp(template);
        if (!created) break;
        directory = [[NSFileManager defaultManager] stringWithFileSystemRepresentation:created length:strlen(created)];
        if (!directory) {
            rmdir(created);
            break;
        }

        path = [directory stringByAppendingPathComponent:@"state.json"];
        initial = jsonData(@{@"revision": @1, @"payload": @[@"before", @2]});
        replacement = jsonData(@{@"revision": @2, @"payload": @[@"complete", @2, @3, @4]});
        syncFailureData = jsonData(@{@"revision": @3, @"payload": @[@"directory-sync-failed"]});
        if (!initial || !replacement || !syncFailureData || !atomicData(path, initial, 0600)) break;
        if (!atomicData(path, replacement, 0600)) break;

        NSData *stored = [NSData dataWithContentsOfFile:path];
        struct stat st;
        if (![stored isEqualToData:replacement] ||
            lstat(path.fileSystemRepresentation, &st) != 0 || !S_ISREG(st.st_mode) ||
            (st.st_mode & 07777) != 0600 ||
            !storageTestDirectoryHasOnly(directory, @"state.json")) break;

        // rename has happened before the injected parent-directory sync failure.
        TestFailDirectorySync = YES;
        if (atomicData(path, syncFailureData, 0600)) break;
        stored = [NSData dataWithContentsOfFile:path];
        if (![stored isEqualToData:syncFailureData] ||
            !storageTestDirectoryHasOnly(directory, @"state.json")) break;

        // unlink has happened before the injected parent-directory sync failure.
        if (removeDurably(path)) break;
        TestFailDirectorySync = NO;
        errno = 0;
        if (lstat(path.fileSystemRepresentation, &st) == 0 || errno != ENOENT ||
            !storageTestDirectoryHasOnly(directory, nil)) break;

        if (!removeDurably(path)) break;
        if (!atomicData(path, replacement, 0600) || !removeDurably(path)) break;
        errno = 0;
        if (lstat(path.fileSystemRepresentation, &st) == 0 || errno != ENOENT ||
            !storageTestDirectoryHasOnly(directory, nil)) break;
        passed = YES;
    } while (0);

    TestFailDirectorySync = NO;
    if (directory && !storageTestCleanupDirectory(directory)) passed = NO;
    return passed;
}
