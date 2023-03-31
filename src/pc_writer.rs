use crate::bs_out::ByteStreamOutBuffer;
use crate::cv_section::CompressedVectorSectionHeader;
use crate::error::Converter;
use crate::packet::DataPacketHeader;
use crate::paged_writer::PagedWriter;
use crate::point::RawPoint;
use crate::Error;
use crate::PointCloud;
use crate::Record;
use crate::Result;
use std::collections::VecDeque;
use std::io::{Read, Seek, Write};

/// Creates a new point cloud by consuming points and writing them into an E57 file.
pub struct PointCloudWriter<'a, T: Read + Write + Seek> {
    writer: &'a mut PagedWriter<T>,
    pointclouds: &'a mut Vec<PointCloud>,
    guid: String,
    section_offset: u64,
    section_header: CompressedVectorSectionHeader,
    prototype: Vec<Record>,
    point_count: u64,
    buffer: VecDeque<RawPoint>,
    max_points_per_packet: usize,
}

impl<'a, T: Read + Write + Seek> PointCloudWriter<'a, T> {
    pub(crate) fn new(
        writer: &'a mut PagedWriter<T>,
        pointclouds: &'a mut Vec<PointCloud>,
        guid: &str,
        prototype: Vec<Record>,
    ) -> Result<Self> {
        let section_offset = writer.physical_position()?;

        let mut section_header = CompressedVectorSectionHeader::default();
        section_header.data_offset = section_offset + CompressedVectorSectionHeader::SIZE;
        section_header.section_length = CompressedVectorSectionHeader::SIZE;
        section_header.write(writer)?;

        // Each data packet can contain up to 2^16 bytes and we need some reserved
        // space for header and bytes that are not yet filled and need to be included later.
        let point_size: usize = prototype.iter().map(|p| p.data_type.bit_size()).sum();
        let max_points_per_packet = (64000 * 8) / point_size;

        Ok(PointCloudWriter {
            writer,
            pointclouds,
            guid: guid.to_owned(),
            section_offset,
            section_header,
            prototype,
            point_count: 0,
            buffer: VecDeque::new(),
            max_points_per_packet,
        })
    }

    fn write_buffer_to_disk(&mut self) -> Result<()> {
        let packet_points = self.max_points_per_packet.min(self.buffer.len());
        if packet_points == 0 {
            return Ok(());
        }

        let prototype_len = self.prototype.len();
        let mut buffers = vec![ByteStreamOutBuffer::new(); prototype_len];
        for _ in 0..packet_points {
            let p = self
                .buffer
                .pop_front()
                .internal_err("Failed to get next point for writing")?;
            for (i, r) in self.prototype.iter().enumerate() {
                let name = &r.name;
                let raw_value = p.get(name).invalid_err(format!(
                    "Point is missing record with name '{}'",
                    name.to_tag_name()
                ))?;
                r.data_type.write(raw_value, &mut buffers[i])?;
            }
        }

        // Check and prepare buffer sizes
        let mut sum_buffer_sizes = 0;
        let mut buffer_sizes = Vec::with_capacity(prototype_len);
        for buffer in &buffers {
            let len = buffer.full_bytes();
            sum_buffer_sizes += len;
            buffer_sizes.push(len as u16);
        }

        // Calculate packet length for header
        let mut packet_length = DataPacketHeader::SIZE + prototype_len * 2 + sum_buffer_sizes;
        if packet_length % 4 != 0 {
            let missing = 4 - (packet_length % 4);
            packet_length += missing;
        }
        if packet_length > u16::MAX as usize {
            Error::internal("Invalid data packet length")?
        }

        // Add data packet length to section length for later
        self.section_header.section_length += packet_length as u64;

        // Write data packet header
        DataPacketHeader {
            comp_restart_flag: false,
            packet_length: packet_length as u64,
            bytestream_count: prototype_len as u16,
        }
        .write(&mut self.writer)?;

        // Write bytestream sizes as u16 values
        for size in buffer_sizes {
            let bytes = size.to_le_bytes();
            self.writer
                .write_all(&bytes)
                .write_err("Cannot write data packet buffer size")?;
        }

        // Write actual bytestream buffers with data
        for buffer in &mut buffers {
            let data = buffer.get_full_bytes();
            self.writer
                .write_all(&data)
                .write_err("Cannot write bytestream buffer into data packet")?;
        }

        self.writer
            .align()
            .write_err("Failed to align writer on next 4-byte offset after writing data packet")?;

        Ok(())
    }

    /// Adds a new point to the point cloud.
    pub fn add_point(&mut self, point: RawPoint) -> Result<()> {
        self.buffer.push_back(point);
        self.point_count += 1;
        if self.buffer.len() >= self.max_points_per_packet {
            self.write_buffer_to_disk()?;
        }
        Ok(())
    }

    /// Called after all points have been added to finalize the creation of the new point cloud.
    pub fn finalize(&mut self) -> Result<()> {
        // Flush remaining points from buffer
        while !self.buffer.is_empty() {
            self.write_buffer_to_disk()?;
        }

        // We need to write the section header again with the final length
        // which was previously unknown and is now available.
        let end_offset = self
            .writer
            .physical_position()
            .write_err("Failed to get section end offset")?;
        self.writer
            .physical_seek(self.section_offset)
            .write_err("Failed to seek to section start for final update")?;
        self.section_header.write(&mut self.writer)?;
        self.writer
            .physical_seek(end_offset)
            .write_err("Failed to seek behind finalized section")?;

        // Add metadata for pointcloud for XML generation later, when the file is completed.
        self.pointclouds.push(PointCloud {
            guid: self.guid.clone(),
            records: self.point_count,
            file_offset: self.section_offset,
            prototype: self.prototype.clone(),
            ..Default::default()
        });

        Ok(())
    }
}
