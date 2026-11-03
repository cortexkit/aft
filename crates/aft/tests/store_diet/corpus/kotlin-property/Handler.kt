class Handler {
    private val auditService: AuditService = AuditService()

    fun handle() {
        auditService.find()
        val userService: UserService = UserService()
        userService.find()
        val billingService = BillingService()
        billingService.find()
    }
}

class UserService { fun find() {} }
class AuditService { fun find() {} }
class BillingService { fun find() {} }
