class EntryPoint {
    private UserService userService;

    void handle() {
        userService.find();
    }
}

class UserService {
    void find() {}
}

class AuditService {
    void find() {}
}
