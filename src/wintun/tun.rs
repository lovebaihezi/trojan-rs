use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use smoltcp::{
    iface::{SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium},
    socket::{
        tcp::{Socket as TcpSocket, SocketBuffer},
        udp::{PacketBuffer, PacketMetadata, Socket as UdpSocket},
        Socket,
    },
    time::Instant,
    wire::{
        IpAddress, IpEndpoint, IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet, TcpPacket, UdpPacket,
    },
};
use wintun::{Packet, Session};

use crate::{
    wintun::{
        ipset::is_private,
        waker::{Event, WakerMode, Wakers},
    },
    OPTIONS,
};

pub struct WintunDevice<'a> {
    session: Arc<Session>,
    sockets: Arc<SocketSet<'a>>,
    tcp_wakers: Wakers,
    udp_wakers: Wakers,
    tcp_set: HashSet<IpEndpoint>,
    udp_set: HashSet<IpEndpoint>,
    mtu: usize,
}

impl<'a> WintunDevice<'a> {
    pub fn new(session: Arc<Session>, mtu: usize, sockets: Arc<SocketSet<'a>>) -> Self {
        Self {
            session,
            mtu,
            sockets,
            tcp_wakers: Wakers::new(),
            udp_wakers: Wakers::new(),
            tcp_set: HashSet::new(),
            udp_set: HashSet::new(),
        }
    }

    pub fn ensure_tcp_socket(&mut self, endpoint: IpEndpoint) {
        if self.tcp_set.contains(&endpoint) {
            return;
        }

        let socket = TcpSocket::new(
            SocketBuffer::new(vec![0; OPTIONS.wintun_args().tcp_rx_buffer_size]),
            SocketBuffer::new(vec![0; OPTIONS.wintun_args().tcp_tx_buffer_size]),
        );
        let sockets = unsafe { Arc::get_mut_unchecked(&mut self.sockets) };
        let handle = sockets.add(socket);
        let socket = sockets.get_mut::<TcpSocket>(handle);
        let (_, tx) = self.tcp_wakers.get_wakers(handle);
        socket.register_send_waker(tx);
        socket.listen(endpoint).unwrap();
        socket.set_nagle_enabled(false);
        socket.set_ack_delay(None);
        self.tcp_set.insert(endpoint);
    }

    pub fn ensure_udp_socket(&mut self, endpoint: IpEndpoint) {
        if self.udp_set.contains(&endpoint) {
            return;
        }
        let mut socket = UdpSocket::new(
            PacketBuffer::new(
                vec![PacketMetadata::EMPTY; OPTIONS.wintun_args().udp_rx_meta_size],
                vec![0; OPTIONS.wintun_args().udp_rx_buffer_size],
            ),
            PacketBuffer::new(
                vec![PacketMetadata::EMPTY; OPTIONS.wintun_args().udp_rx_meta_size],
                vec![0; OPTIONS.wintun_args().udp_tx_buffer_size],
            ),
        );
        socket.bind(endpoint).unwrap();
        let sockets = unsafe { Arc::get_mut_unchecked(&mut self.sockets) };
        let handle = sockets.add(socket);
        log::info!("udp handle:{} is {}", handle, endpoint);
        let socket = sockets.get_mut::<UdpSocket>(handle);
        let (rx, tx) = self.udp_wakers.get_wakers(handle);
        socket.register_recv_waker(rx);
        socket.register_send_waker(tx);
        self.udp_set.insert(endpoint);
    }

    pub fn remove_socket(&mut self, handle: SocketHandle) {
        let sockets = unsafe { Arc::get_mut_unchecked(&mut self.sockets) };
        if let None = sockets
            .iter_mut()
            .find(|(handle1, _)| handle == *handle1)
            .map(|(_, socket)| match socket {
                Socket::Udp(socket) => {
                    let endpoint = socket.endpoint();
                    let endpoint = IpEndpoint::new(endpoint.addr.unwrap(), endpoint.port);
                    self.udp_set.remove(&endpoint);
                    socket.register_send_waker(self.udp_wakers.get_dummy_waker());
                    socket.register_recv_waker(self.udp_wakers.get_dummy_waker());
                    socket.close();
                }
                Socket::Tcp(socket) => {
                    let endpoint = socket.local_endpoint().unwrap();
                    self.tcp_set.remove(&endpoint);
                }
                _ => {}
            })
        {
            log::error!("handle:{} not found in socket set", handle);
        }

        sockets.remove(handle);
    }

    pub fn get_udp_events(&self) -> HashMap<SocketHandle, Event> {
        self.udp_wakers.get_events()
    }

    pub fn get_tcp_events(&self) -> HashMap<SocketHandle, Event> {
        self.tcp_wakers.get_events()
    }

    pub fn get_tcp_socket_mut(&mut self, handle: SocketHandle, waker: WakerMode) -> &mut TcpSocket {
        let sockets = unsafe { Arc::get_mut_unchecked(&mut self.sockets) };
        let socket: &mut TcpSocket = sockets.get_mut(handle);
        match waker {
            WakerMode::Recv => {
                let (rx, _) = self.tcp_wakers.get_wakers(handle);
                socket.register_recv_waker(rx);
            }
            WakerMode::Send => {
                let (_, tx) = self.tcp_wakers.get_wakers(handle);
                socket.register_send_waker(tx);
            }
            WakerMode::Both => {
                let (rx, tx) = self.tcp_wakers.get_wakers(handle);
                socket.register_recv_waker(rx);
                socket.register_send_waker(tx);
            }
            WakerMode::Dummy => {
                let waker = self.tcp_wakers.get_dummy_waker();
                socket.register_recv_waker(waker);
                socket.register_send_waker(waker);
            }
            WakerMode::None => {}
        }
        unsafe { std::mem::transmute(socket) }
    }

    pub fn get_udp_socket_mut(&mut self, handle: SocketHandle, waker: WakerMode) -> &mut UdpSocket {
        let sockets = unsafe { Arc::get_mut_unchecked(&mut self.sockets) };
        let socket: &mut UdpSocket = sockets.get_mut(handle);
        match waker {
            WakerMode::Recv => {
                let (rx, _) = self.udp_wakers.get_wakers(handle);
                socket.register_recv_waker(rx);
            }
            WakerMode::Send => {
                let (_, tx) = self.udp_wakers.get_wakers(handle);
                socket.register_send_waker(tx);
            }
            WakerMode::Both => {
                let (rx, tx) = self.udp_wakers.get_wakers(handle);
                socket.register_recv_waker(rx);
                socket.register_send_waker(tx);
            }
            WakerMode::None => {}
            WakerMode::Dummy => {
                let waker = self.udp_wakers.get_dummy_waker();
                socket.register_send_waker(waker);
                socket.register_recv_waker(waker);
            }
        }
        unsafe { std::mem::transmute(socket) }
    }
}

impl<'b> Device for WintunDevice<'b> {
    type RxToken<'a> = RxToken where Self:'a;
    type TxToken<'a> = TxToken where Self:'a;

    fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.session
            .try_receive()
            .ok()
            .map(|packet| {
                packet.map(|packet| {
                    preprocess_packet(&packet, self);
                    let rx = RxToken { packet };
                    let tx = TxToken {
                        session: self.session.clone(),
                    };
                    (rx, tx)
                })
            })
            .unwrap_or(None)
    }

    fn transmit(&mut self, _: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            session: self.session.clone(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut dc = DeviceCapabilities::default();
        dc.medium = Medium::Ip;
        dc.max_transmission_unit = self.mtu;
        dc
    }
}

fn preprocess_packet(packet: &Packet, device: &mut WintunDevice) {
    let (dst_addr, payload, protocol) = match IpVersion::of_packet(packet.bytes()).unwrap() {
        IpVersion::Ipv4 => {
            let packet = Ipv4Packet::new_checked(packet.bytes()).unwrap();
            let dst_addr = packet.dst_addr();
            (
                IpAddress::Ipv4(dst_addr),
                packet.payload(),
                packet.next_header(),
            )
        }
        IpVersion::Ipv6 => {
            let packet = Ipv6Packet::new_checked(packet.bytes()).unwrap();
            let dst_addr = packet.dst_addr();
            (
                IpAddress::Ipv6(dst_addr),
                packet.payload(),
                packet.next_header(),
            )
        }
    };
    let (dst_port, connect) = match protocol {
        IpProtocol::Udp => {
            let packet = UdpPacket::new_checked(payload).unwrap();
            (packet.dst_port(), None)
        }
        IpProtocol::Tcp => {
            let packet = TcpPacket::new_checked(payload).unwrap();
            (packet.dst_port(), Some(packet.syn() && !packet.ack()))
        }
        _ => return,
    };

    let dst_endpoint = IpEndpoint::new(dst_addr, dst_port);
    if is_private(dst_endpoint) {
        return;
    }

    match connect {
        Some(true) => {
            device.ensure_tcp_socket(dst_endpoint);
        }
        None => {
            device.ensure_udp_socket(dst_endpoint);
        }
        _ => {}
    }
}

pub struct TxToken {
    session: Arc<Session>,
}

pub struct RxToken {
    packet: Packet,
}

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.packet.bytes_mut())
    }
}

impl smoltcp::phy::TxToken for TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        self.session
            .allocate_send_packet(len as u16)
            .map(|mut packet| {
                let r = f(packet.bytes_mut());
                self.session.send_packet(packet);
                r
            })
            .unwrap()
    }
}
