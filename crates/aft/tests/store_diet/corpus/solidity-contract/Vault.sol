// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
}

library SafeMath {
    function add(uint256 a, uint256 b) internal pure returns (uint256) {
        return a + b;
    }
}

contract Vault {
    address public owner;
    uint256 private _totalDeposits;

    event Deposit(address indexed from, uint256 amount);
    error Unauthorized(address caller);

    struct UserInfo {
        uint256 balance;
        uint256 lastUpdate;
    }

    enum Status { Active, Paused, Closed }

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized(msg.sender);
        _;
    }

    constructor(address initialOwner) {
        owner = initialOwner;
    }

    function deposit() external payable {
        _totalDeposits += msg.value;
        emit Deposit(msg.sender, msg.value);
    }

    function withdraw(uint256 amount) external onlyOwner {
        payable(owner).transfer(amount);
    }
}
