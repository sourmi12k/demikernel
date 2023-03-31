// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//======================================================================================================================
// Imports
//======================================================================================================================

use anyhow::Result;
use demikernel::{
    runtime::fail::Fail,
    LibOS,
    QDesc,
    QToken,
};
use std::{
    net::{
        Ipv4Addr,
        SocketAddrV4,
    },
    time::Duration,
};

//======================================================================================================================
// Constants
//======================================================================================================================

#[cfg(target_os = "windows")]
pub const AF_INET: i32 = windows::Win32::Networking::WinSock::AF_INET.0 as i32;

#[cfg(target_os = "windows")]
pub const SOCK_STREAM: i32 = windows::Win32::Networking::WinSock::SOCK_STREAM as i32;

#[cfg(target_os = "linux")]
pub const AF_INET: i32 = libc::AF_INET;

#[cfg(target_os = "linux")]
pub const SOCK_STREAM: i32 = libc::SOCK_STREAM;

//======================================================================================================================
// Standalone Functions
//======================================================================================================================

/// Drives integration tests for connect() on TCP sockets.
pub fn run(libos: &mut LibOS, local: &SocketAddrV4, remote: &SocketAddrV4) -> Result<()> {
    connect_invalid_queue_descriptor(libos, remote)?;
    connect_to_bad_remote(libos)?;
    connect_unbound_socket(libos, remote)?;
    connect_bound_socket(libos, local, remote)?;
    connect_listening_socket(libos, local, remote)?;
    connect_connecting_socket(libos, remote)?;
    connect_accepting_socket(libos, local, remote)?;
    connect_closed_socket(libos, remote)?;

    Ok(())
}

/// Attempts to connect an invalid queue descriptor.
fn connect_invalid_queue_descriptor(libos: &mut LibOS, remote: &SocketAddrV4) -> Result<()> {
    println!("{}", stringify!(connect_invalid_queue_descriptor));

    // Fail to connect().
    let e: Fail = libos
        .connect(QDesc::from(0), remote.to_owned())
        .expect_err("connect() an invalid socket should fail");

    // Sanity check error code.
    assert_eq!(e.errno, libc::EBADF, "connect() failed with {}", e.cause);

    Ok(())
}

/// Attempts to connect a TCP socket that is not bound.
fn connect_unbound_socket(libos: &mut LibOS, remote: &SocketAddrV4) -> Result<()> {
    println!("{}", stringify!(connect_unbound_socket));

    // Create an unbound socket.
    let sockqd: QDesc = libos.socket(AF_INET, SOCK_STREAM, 0)?;

    // Succeed to connect socket.
    let qt: QToken = libos.connect(sockqd, remote.to_owned())?;

    // Poll once to ensure that the connect() co-routine runs.
    match libos.wait(qt, Some(Duration::from_micros(0))) {
        Err(e) if e.errno == libc::ETIMEDOUT => {},
        Ok(_) => anyhow::bail!("wait() should not succeed"),
        Err(_) => anyhow::bail!("wait() should timeout"),
    }

    // Succeed to close socket.
    libos.close(sockqd)?;

    Ok(())
}

/// Attempts to connect a TCP socket to a remote that is not accepting connections.
fn connect_to_bad_remote(libos: &mut LibOS) -> Result<()> {
    println!("{}", stringify!(connect_to_bad_remote));

    // Create an unbound socket.
    let sockqd: QDesc = libos.socket(AF_INET, SOCK_STREAM, 0)?;

    // Bad remote address (any localhost port).
    let remote: SocketAddrV4 = {
        let ipv4: Ipv4Addr = Ipv4Addr::UNSPECIFIED;
        SocketAddrV4::new(ipv4, 0)
    };

    // Succeed to connect socket.
    let qt: QToken = libos.connect(sockqd, remote)?;

    // Poll for enough time to get the connection refused.
    match libos.wait(qt, Some(Duration::from_secs(75))) {
        Ok(qr) if qr.qr_ret == libc::ECONNREFUSED => {},
        Ok(_) => anyhow::bail!("wait() should return ECONNREFUSED"),
        Err(e) if e.errno == libc::ETIMEDOUT => anyhow::bail!("wait() should not timeout"),
        Err(_) => anyhow::bail!("wait() should not fail"),
    }

    // Succeed to close socket.
    libos.close(sockqd)?;

    Ok(())
}

/// Attempts to connect a TCP socket that is bound.
fn connect_bound_socket(libos: &mut LibOS, local: &SocketAddrV4, remote: &SocketAddrV4) -> Result<()> {
    println!("{}", stringify!(connect_bound_socket));

    // Create a bound socket.
    let sockqd: QDesc = libos.socket(AF_INET, SOCK_STREAM, 0)?;
    libos.bind(sockqd, local.to_owned())?;

    // Succeed to connect socket.
    let qt: QToken = libos.connect(sockqd, remote.to_owned())?;

    // Poll once to ensure that the connect() co-routine runs.
    match libos.wait(qt, Some(Duration::from_micros(0))) {
        Err(e) if e.errno == libc::ETIMEDOUT => {},
        Ok(_) => anyhow::bail!("wait() should not succeed"),
        Err(_) => anyhow::bail!("wait() should timeout"),
    }

    // Succeed to close socket.
    libos.close(sockqd)?;

    Ok(())
}

/// Attempts to connect a TCP socket that is listening.
fn connect_listening_socket(libos: &mut LibOS, local: &SocketAddrV4, remote: &SocketAddrV4) -> Result<()> {
    println!("{}", stringify!(connect_listening_socket));

    // Create a listening socket.
    let sockqd: QDesc = libos.socket(AF_INET, SOCK_STREAM, 0)?;
    libos.bind(sockqd, local.to_owned())?;
    libos.listen(sockqd, 16)?;

    // Fail to connect().
    let e: Fail = libos
        .connect(sockqd, remote.to_owned())
        .expect_err("connect() a socket that is listening should fail");

    // Sanity check error code.
    assert_eq!(e.errno, libc::EOPNOTSUPP, "connect() failed with {}", e.cause);

    // Succeed to close socket.
    libos.close(sockqd)?;

    Ok(())
}

/// Attempts to connect a TCP socket that is already connecting.
fn connect_connecting_socket(libos: &mut LibOS, remote: &SocketAddrV4) -> Result<()> {
    println!("{}", stringify!(connect_connecting_socket));

    // Create a connecting socket.
    let sockqd: QDesc = libos.socket(AF_INET, SOCK_STREAM, 0)?;
    let qt: QToken = libos.connect(sockqd, remote.to_owned())?;

    // Poll once to ensure that the connect() co-routine runs.
    match libos.wait(qt, Some(Duration::from_micros(0))) {
        Err(e) if e.errno == libc::ETIMEDOUT => {},
        Ok(_) => anyhow::bail!("wait() should not succeed"),
        Err(_) => anyhow::bail!("wait() should timeout"),
    }

    // Fail to connect().
    let e: Fail = libos
        .connect(sockqd, remote.to_owned())
        .expect_err("connect() a socket that is connecting should fail");

    // Sanity check error code.
    assert_eq!(e.errno, libc::EINPROGRESS, "connect() failed with {}", e.cause);

    // Succeed to close socket.
    libos.close(sockqd)?;

    Ok(())
}

/// Attempts to connect a TCP socket that is accepting connections.
fn connect_accepting_socket(libos: &mut LibOS, local: &SocketAddrV4, remote: &SocketAddrV4) -> Result<()> {
    println!("{}", stringify!(connect_accepting_socket));

    // Create an accepting socket.
    let sockqd: QDesc = libos.socket(AF_INET, SOCK_STREAM, 0)?;
    libos.bind(sockqd, local.to_owned())?;
    libos.listen(sockqd, 16)?;
    let qt: QToken = libos.accept(sockqd)?;

    // Poll once to ensure that the accept() co-routine runs.
    match libos.wait(qt, Some(Duration::from_micros(0))) {
        Err(e) if e.errno == libc::ETIMEDOUT => {},
        Ok(_) => anyhow::bail!("wait() should not succeed"),
        Err(_) => anyhow::bail!("wait() should timeout"),
    }

    // Fail to connect().
    let e: Fail = libos
        .connect(sockqd, remote.to_owned())
        .expect_err("connect() a socket that is accepting should fail");

    // Sanity check error code.
    assert_eq!(e.errno, libc::EOPNOTSUPP, "connect() failed with {}", e.cause);

    // Succeed to close socket.
    libos.close(sockqd)?;

    Ok(())
}

/// Attempts to connect a TCP socket that is closed.
fn connect_closed_socket(libos: &mut LibOS, remote: &SocketAddrV4) -> Result<()> {
    println!("{}", stringify!(connect_closed_socket));

    // Create a closed socket.
    let sockqd: QDesc = libos.socket(AF_INET, SOCK_STREAM, 0)?;
    libos.close(sockqd)?;

    // Fail to connect().
    let e: Fail = libos
        .connect(sockqd, remote.to_owned())
        .expect_err("connect() a closed socket should fail");

    // Sanity check error code.
    assert_eq!(e.errno, libc::EBADF, "connect() failed with {}", e.cause);

    Ok(())
}
