use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

const STARTUP_DELAY: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const OPTIONS_CAP: usize = 2048;
const DESCRIBE_CAP: usize = 4096;

pub fn probe(port: u16, host: Option<Ipv4Addr>) {
    std::thread::sleep(STARTUP_DELAY);

    let ip = host.unwrap_or(Ipv4Addr::LOCALHOST);
    let addr = SocketAddr::from((ip, port));
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(e) => {
            logging::log!("selftest: connect failed: {e}");
            return;
        }
    };

    if let Err(e) = stream.set_read_timeout(Some(IO_TIMEOUT)) {
        logging::log!("selftest: set_read_timeout failed: {e}");
        return;
    }
    if let Err(e) = stream.set_write_timeout(Some(IO_TIMEOUT)) {
        logging::log!("selftest: set_write_timeout failed: {e}");
        return;
    }

    if let Err(e) = stream.write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n") {
        logging::log!("selftest: OPTIONS write failed: {e}");
        return;
    }
    let options = match read_message(&mut stream, OPTIONS_CAP) {
        Ok(text) => text,
        Err(e) => {
            logging::log!("selftest: OPTIONS read failed: {e}");
            return;
        }
    };
    logging::log!("selftest: OPTIONS status={}", status_line(&options));

    let describe = format!(
        "DESCRIBE rtsp://{ip}:{port}/live RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n"
    );
    if let Err(e) = stream.write_all(describe.as_bytes()) {
        logging::log!("selftest: DESCRIBE write failed: {e}");
        return;
    }
    let response = match read_message(&mut stream, DESCRIBE_CAP) {
        Ok(text) => text,
        Err(e) => {
            logging::log!("selftest: DESCRIBE read failed: {e}");
            return;
        }
    };

    let sdp = sdp_body(&response);
    logging::log!("selftest: SDP={sdp}");

    let (jpeg, pcmu) = has_jpeg_and_pcmu(&sdp);
    if !jpeg {
        logging::log!("selftest: FAIL: no JPEG/90000");
    }
    if !pcmu {
        logging::log!("selftest: FAIL: no PCMU/8000");
    }
    if jpeg && pcmu {
        logging::log!("selftest: PASS");
    }
}

fn status_line(response: &str) -> &str {
    response
        .lines()
        .next()
        .map_or("", |line| line.trim_end_matches('\r'))
}

fn sdp_body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map_or("", |(_, body)| body)
}

fn has_jpeg_and_pcmu(sdp: &str) -> (bool, bool) {
    (sdp.contains("JPEG/90000"), sdp.contains("PCMU/8000"))
}

fn read_message(stream: &mut TcpStream, cap: usize) -> io::Result<String> {
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 512];
    let mut header_end: Option<usize> = None;
    let mut body_len: usize = 0;

    loop {
        if data.len() >= cap {
            break;
        }
        let chunk = (cap - data.len()).min(buf.len());
        let read = match stream.read(&mut buf[..chunk]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(e) => return Err(e),
        };

        data.extend_from_slice(&buf[..read]);
        if header_end.is_none() {
            if let Some(pos) = find_subsequence(&data, b"\r\n\r\n") {
                header_end = Some(pos + 4);
                body_len = header_content_length(&data[..pos]);
            }
        }
        if let Some(end) = header_end {
            if body_len == 0 || data.len() >= end + body_len {
                break;
            }
        }
    }

    Ok(String::from_utf8_lossy(&data).into_owned())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn header_content_length(headers: &[u8]) -> usize {
    let head = String::from_utf8_lossy(headers);
    for line in head.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::has_jpeg_and_pcmu;

    #[test]
    fn detects_jpeg_and_pcmu_rtpmap() {
        let sdp = concat!(
            "v=0\r\n",
            "m=video 0 RTP/AVP 96\r\n",
            "a=rtpmap:96 JPEG/90000\r\n",
            "m=audio 0 RTP/AVP 0\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
        );
        assert_eq!(has_jpeg_and_pcmu(sdp), (true, true));
    }

    #[test]
    fn reports_missing_codecs() {
        assert_eq!(has_jpeg_and_pcmu("v=0\r\n"), (false, false));
        assert_eq!(has_jpeg_and_pcmu("a=rtpmap:96 JPEG/90000"), (true, false));
        assert_eq!(has_jpeg_and_pcmu("a=rtpmap:0 PCMU/8000"), (false, true));
    }
}
