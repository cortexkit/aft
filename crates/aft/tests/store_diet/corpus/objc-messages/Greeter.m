
@interface CKGreeter : NSObject
@property (nonatomic, copy) NSString *title;
+ (instancetype)sharedGreeter;
- (NSString *)greetWithName:(NSString *)name punctuation:(NSString *)punctuation;
@end

@protocol CKRunnable
@property (nonatomic, assign) BOOL enabled;
- (void)run;
@end

typedef NSInteger CKCount;

static NSString *CKFormatName(NSString *name) {
  return name;
}

void CKSendMessage(id obj, id bar) {
  [obj foo:bar];
}

@implementation CKGreeter
+ (instancetype)sharedGreeter {
  return [CKGreeter new];
}

- (NSString *)greetWithName:(NSString *)name punctuation:(NSString *)punctuation {
  NSString *message = [self format:name punctuation:punctuation];
  return message;
}

- (NSString *)format:(NSString *)value punctuation:(NSString *)punctuation {
  return [NSString stringWithFormat:@"%@%@", value, punctuation];
}
@end
